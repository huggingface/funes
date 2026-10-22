//! The inference backend behind funes' two model operations — embedding and reranking. The rest of
//! funes talks to these traits via the [`embedder`]/[`reranker`] factories, never a concrete ML
//! stack, so an alternative backend slots in behind the same interface. The backend is chosen at
//! build time in one place — the `Default*` aliases below: default build → BLAS (a from-scratch
//! forward on Accelerate/faer); `--no-default-features --features onnx` → fastembed/ort. On macOS
//! the default build also carries `metal`, the same encoders on the GPU, which [`embedder`] and
//! [`reranker`] hand their bulk calls to (see [`metal::Hybrid`] and [`metal::HybridReranker`]).

#[cfg(feature = "blas")]
pub mod blas;
#[cfg(all(feature = "metal", target_os = "macos"))]
pub mod metal;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

// The single backend-selection point. One of these alias pairs is compiled; the factories below
// box whichever it names. When both backends are compiled in (the backend benchmark builds that
// way to use ONNX as its reference), BLAS is the one funes runs.
#[cfg(all(feature = "onnx", not(feature = "blas")))]
use self::{OnnxEmbedder as DefaultEmbedder, OnnxReranker as DefaultReranker};
#[cfg(feature = "blas")]
use blas::{BlasEmbedder as DefaultEmbedder, GteReranker as DefaultReranker};
#[cfg(not(any(feature = "blas", feature = "onnx")))]
compile_error!("funes needs an inference backend: feature `blas` (default) or `onnx`");

/// How many texts one `embed` call takes.
const EMBED_BATCH: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmbeddingModel {
    BgeSmallEn,
    MultilingualE5Small,
}

impl EmbeddingModel {
    pub const ALL: [EmbeddingModel; 2] = [EmbeddingModel::BgeSmallEn, EmbeddingModel::MultilingualE5Small];

    pub const fn id(self) -> &'static str {
        match self {
            EmbeddingModel::BgeSmallEn => "BAAI/bge-small-en-v1.5",
            EmbeddingModel::MultilingualE5Small => "intfloat/multilingual-e5-small",
        }
    }

    pub const fn from_id(id: &str) -> Option<EmbeddingModel> {
        let mut i = 0;
        while i < EmbeddingModel::ALL.len() {
            if same_bytes(EmbeddingModel::ALL[i].id().as_bytes(), id.as_bytes()) {
                return Some(EmbeddingModel::ALL[i]);
            }
            i += 1;
        }
        None
    }

    /// e5 was trained with these query and passage prefixes, and its model card asks for them.
    fn prefixes(self) -> (&'static str, &'static str) {
        match self {
            EmbeddingModel::BgeSmallEn => ("", ""),
            EmbeddingModel::MultilingualE5Small => ("query: ", "passage: "),
        }
    }
}

const fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Embed each text into a dense vector, in input order.
pub trait Embedder: Send {
    fn model(&self) -> EmbeddingModel;

    /// Encode each text as given, in input order.
    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// How many texts a call should carry. A caller reports progress and checks its budget between
    /// calls, so this is about a few seconds' work; a GPU wants many texts per call to stay busy.
    fn batch_size(&self) -> usize {
        EMBED_BATCH
    }

    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefix = self.model().prefixes().1;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        self.encode(&prefixed.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        let prefixed = format!("{}{query}", self.model().prefixes().0);
        self.encode(&[prefixed.as_str()])?.pop().context("empty embedding")
    }
}

/// Score each doc against the query; one score per doc, in input order (higher = more relevant).
pub trait Reranker: Send {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
}

/// Build the embedder for `model` on the compiled-in backend. Call sites use this instead of
/// naming a concrete type, so the backend is decided only by the `Default*` alias above.
pub fn embedder(model: EmbeddingModel) -> Result<Box<dyn Embedder>> {
    #[cfg(all(feature = "metal", target_os = "macos"))]
    return Ok(Box::new(metal::Hybrid::new(model)?));
    #[allow(unreachable_code)]
    Ok(Box::new(DefaultEmbedder::new(model)?))
}

/// Build the reranker for the compiled-in backend. See [`embedder`].
pub fn reranker() -> Result<Box<dyn Reranker>> {
    #[cfg(all(feature = "metal", target_os = "macos"))]
    return Ok(Box::new(metal::HybridReranker::new()?));
    #[allow(unreachable_code)]
    Ok(Box::new(DefaultReranker::new()?))
}

/// Resolve a snapshot dir for `repo` (e.g. "BAAI/bge-small-en-v1.5") in the standard HF cache,
/// downloading `files`, paths within the repo, on first use.
fn snapshot(repo: &str, files: &[&str]) -> Result<PathBuf> {
    if let Some(dir) = local_snapshot(repo, files) {
        return Ok(dir);
    }
    eprintln!("downloading {repo} (first use)…");
    // A dedicated thread: the blocking hf-hub client drives its own runtime with block_on,
    // which would panic if called from a thread already inside a tokio runtime (the MCP server).
    let owned = repo.to_string();
    let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    std::thread::spawn(move || -> Result<PathBuf> {
        let (owner, name) = owned
            .split_once('/')
            .ok_or_else(|| anyhow!("model repo {owned} is not owner/name"))?;
        let client = hf_hub::HFClientSync::new().map_err(|e| anyhow!("hf-hub client: {e}"))?;
        let r = client.model(owner, name);
        let mut dir = PathBuf::new();
        for file in &files {
            let path = r
                .download_file()
                .filename(file)
                .send()
                .with_context(|| format!("downloading {owned}/{file}"))?;
            // The snapshot holds the file as deep as its path in the repo goes: onnx/model.onnx
            // is two levels down.
            dir = path
                .ancestors()
                .nth(Path::new(file).components().count())
                .ok_or_else(|| anyhow!("{} is not in a snapshot dir", path.display()))?
                .to_path_buf();
        }
        Ok(dir)
    })
    .join()
    .map_err(|_| anyhow!("model download thread panicked"))?
}

/// A cached snapshot of `repo` holding each of `files`, if any. With several snapshots cached the
/// pick is arbitrary, which is fine here: any complete copy of these frozen model repos is
/// interchangeable.
fn local_snapshot(repo: &str, files: &[&str]) -> Option<PathBuf> {
    let base = std::env::var("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/huggingface"));
    let snaps = base
        .join("hub")
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots");
    for e in std::fs::read_dir(&snaps).ok()?.flatten() {
        let p = e.path();
        if files.iter().all(|f| p.join(f).exists()) {
            return Some(p);
        }
    }
    None
}

/// fastembed/ort embedder on the ONNX Runtime CPU EP.
#[cfg(feature = "onnx")]
pub struct OnnxEmbedder {
    model: EmbeddingModel,
    inner: fastembed::TextEmbedding,
}

#[cfg(feature = "onnx")]
impl OnnxEmbedder {
    pub fn new(model: EmbeddingModel) -> Result<Self> {
        use fastembed::{InitOptions, TextEmbedding};
        let fastembed_model = match model {
            EmbeddingModel::BgeSmallEn => fastembed::EmbeddingModel::BGESmallENV15,
            EmbeddingModel::MultilingualE5Small => fastembed::EmbeddingModel::MultilingualE5Small,
        };
        Ok(Self {
            model,
            inner: TextEmbedding::try_new(InitOptions::new(fastembed_model))?,
        })
    }
}

#[cfg(feature = "onnx")]
impl Embedder for OnnxEmbedder {
    fn model(&self) -> EmbeddingModel {
        self.model
    }

    fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed(texts, None)
    }
}

/// fastembed/ort reranker: the gte-reranker-modernbert-base cross-encoder, from the ONNX export its
/// repo ships, on the ONNX Runtime CPU EP.
#[cfg(feature = "onnx")]
pub struct OnnxReranker(fastembed::TextRerank);

#[cfg(feature = "onnx")]
impl OnnxReranker {
    pub fn new() -> Result<Self> {
        use fastembed::{RerankInitOptionsUserDefined, TextRerank, TokenizerFiles, UserDefinedRerankingModel};
        // fastembed does not list gte: funes fetches the export, and the four files fastembed
        // builds its tokenizer from.
        let dir = snapshot(
            "Alibaba-NLP/gte-reranker-modernbert-base",
            &[
                "onnx/model.onnx",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        )?;
        let read =
            |file: &str| std::fs::read(dir.join(file)).with_context(|| format!("reading {file} of {}", dir.display()));
        let tokenizer = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let model = UserDefinedRerankingModel::new(dir.join("onnx/model.onnx"), tokenizer);
        // The 512-token cap the BLAS forward cuts pairs at.
        let options = RerankInitOptionsUserDefined::new().with_max_length(512);
        Ok(Self(TextRerank::try_new_from_user_defined(model, options)?))
    }
}

#[cfg(feature = "onnx")]
impl Reranker for OnnxReranker {
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        // fastembed returns results carrying the original index; project back to input order.
        let mut scores = vec![0f32; docs.len()];
        for r in self.0.rerank(query, docs, false, None)? {
            scores[r.index] = r.score;
        }
        Ok(scores)
    }
}

/// Embed `texts` in batches of the embedder's [`Embedder::batch_size`], calling
/// `on_batch(embedded_so_far)` after each so a caller can report progress (or pass a no-op).
pub(crate) fn embed_batched(
    embedder: &mut dyn Embedder,
    texts: &[&str],
    mut on_batch: impl FnMut(usize),
) -> Result<Vec<Vec<f32>>> {
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    // Asked per call: the first call may bring up a faster backend that wants larger ones.
    while vectors.len() < texts.len() {
        let end = texts.len().min(vectors.len() + embedder.batch_size().max(1));
        let batch = embedder.embed(&texts[vectors.len()..end])?;
        anyhow::ensure!(
            batch.len() == end - vectors.len(),
            "the embedder returned {} vector(s) for {} text(s)",
            batch.len(),
            end - vectors.len()
        );
        vectors.extend(batch);
        on_batch(vectors.len());
    }
    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder {
        model: EmbeddingModel,
        seen: Vec<String>,
    }

    impl Embedder for Recorder {
        fn model(&self) -> EmbeddingModel {
            self.model
        }

        fn encode(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.seen.extend(texts.iter().map(|t| t.to_string()));
            Ok(vec![vec![0.0]; texts.len()])
        }
    }

    #[test]
    fn a_model_is_found_by_its_id_and_only_by_it() {
        for m in EmbeddingModel::ALL {
            assert_eq!(EmbeddingModel::from_id(m.id()), Some(m));
        }
        assert_eq!(EmbeddingModel::from_id("BAAI/bge-small-zh-v1.5"), None);
    }

    #[test]
    fn e5_encodes_prefixed_queries_and_passages() {
        let mut e = Recorder {
            model: EmbeddingModel::MultilingualE5Small,
            seen: Vec::new(),
        };
        e.embed(&["一个段落"]).unwrap();
        e.embed_query("一个问题").unwrap();
        assert_eq!(e.seen, ["passage: 一个段落", "query: 一个问题"]);
    }

    #[test]
    fn bge_encodes_queries_and_passages_as_given() {
        let mut e = Recorder {
            model: EmbeddingModel::BgeSmallEn,
            seen: Vec::new(),
        };
        e.embed(&["a passage"]).unwrap();
        e.embed_query("a question").unwrap();
        assert_eq!(e.seen, ["a passage", "a question"]);
    }
}
