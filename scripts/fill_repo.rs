//! Fill the `repo` column for every session a memory can attribute — local or remote. Where
//! `backfill_repo` resolved each session's checkout with `git` on this machine, this one also
//! attributes the sessions whose checkout is unreachable here (another host, a deleted worktree) by
//! matching their `workdir` against the repo names the memory already resolved. Rewrites only the
//! `repo` column: drop + re-add, vectors and indexes untouched.
//!
//!   cargo run --example fill_repo -- [--apply] [--memory <org/repo>|local]
//!
//! Dry-run by default: prints what it would set, grouped, and what stays empty.
//!
//! Disposable: delete this file and its `[[example]]` entry once resolution happens at capture time.

use anyhow::{bail, Context, Result};
use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use funes::memory::{dataset, lock, remote, Memory};
use funes::traces::repo;
use funes::hub;
use hf_hub::HFClient;
use lance::dataset::{BatchUDF, Dataset, NewColumnTransform};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

/// Workdirs whose checkout no repo name in the memory can match, resolved by hand. A directory keeps
/// the name it was cloned under, so a renamed repo needs its current identity spelled out:
/// `torch-neuronx-evaluation` was renamed to `torch-neuronx-transformers` (the session's own
/// `git remote -v` names it), and the `dotfiles` clone is gone from disk.
const OVERRIDES: &[(&str, &str)] = &[
    ("-torch-neuronx-evaluation", "huggingface/torch-neuronx-transformers"),
    ("-dev-dotfiles", "dacorvo/dotfiles"),
    ("-llama-cpp", "dacorvo/llama.cpp"),
];

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let apply = args.iter().any(|a| a == "--apply");
    let spec = args
        .iter()
        .position(|a| a == "--memory")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let memory = Memory::resolve(spec);

    let ds = memory.open().await.context("opening the memory")?;
    if arrow_schema::Schema::from(ds.schema())
        .column_with_name("repo")
        .is_none()
    {
        bail!("this memory has no `repo` column yet — run `backfill_repo` first");
    }

    let plan = plan(&ds).await?;
    drop(ds);
    report(&plan);
    if !apply {
        println!("\ndry run — pass --apply to write");
        return Ok(());
    }
    if plan.fills.is_empty() {
        println!("\nnothing to fill");
        return Ok(());
    }

    let n = plan.fills.len();
    match &memory {
        Memory::Local { .. } => {
            let _lock = lock::MemoryLock::acquire()?;
            let uri = dataset::table_uri(&dataset::local_memory_dir());
            let mut ds = dataset::open(&uri, HashMap::new()).await?;
            ds.drop_columns(&["repo"]).await?;
            ds.add_columns(
                transform(plan.final_repos.clone()),
                Some(vec!["session_id".to_string()]),
                None,
            )
            .await?;
            println!("\nfilled `repo` for {n} session(s) in the local memory");
        }
        Memory::Remote { uri } => {
            let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to commit")?;
            let (owner, name, _prefix) = hub::parse_hf(uri)?;
            let dataset_uri = format!("{uri}/{}.lance", dataset::TABLE);
            let rev = "main".to_string();
            let opts = HashMap::from([
                ("hf_token".to_string(), token.clone()),
                ("revision".to_string(), rev.clone()),
            ]);
            let handle = HFClient::builder().token(token).build()?.dataset(owner, name);
            let oid = remote::replace_column(
                &handle,
                &dataset_uri,
                opts.clone(),
                &rev,
                format!("backfill: attribute {n} session(s) by workdir"),
                "repo",
                transform(plan.final_repos.clone()),
                vec!["session_id".to_string()],
            )
            .await?;
            // Verify at the new head: a reader must now see the filled values.
            let ds = dataset::open(&dataset_uri, opts).await?;
            let after = plan_from(&ds).await?;
            println!(
                "\nfilled `repo` for {n} session(s) in commit {oid}; {} session(s) still unattributed",
                after.unattributed.values().sum::<usize>()
            );
        }
    }
    Ok(())
}

struct Plan {
    /// session_id → repo we would newly set
    fills: BTreeMap<String, String>,
    /// session_id → repo for every session (existing values kept)
    final_repos: HashMap<String, String>,
    /// sessions with no attributable checkout, by workdir
    unattributed: BTreeMap<String, usize>,
}

async fn plan(ds: &Dataset) -> Result<Plan> {
    plan_from(ds).await
}

/// Resolve every session: keep a non-empty `repo`, else try this machine's checkout, else match the
/// `workdir` against the repo names the memory already knows — longest name first, so
/// `…-dev-funes-viz` attributes to `funes-viz` and not to `funes`.
async fn plan_from(ds: &Dataset) -> Result<Plan> {
    let batches =
        dataset::scan_rows(ds, &["session_id", "workdir", "repo", "source_path"], None, None).await?;
    let mut known: BTreeMap<String, (String, String)> = BTreeMap::new(); // sid → (workdir, source)
    let mut have: HashMap<String, String> = HashMap::new(); // sid → existing repo
    for b in &batches {
        let (sid, wd, rp, src) = (
            col(b, "session_id")?,
            col(b, "workdir")?,
            col(b, "repo")?,
            col(b, "source_path")?,
        );
        for i in 0..b.num_rows() {
            let s = sid.value(i).to_string();
            known
                .entry(s.clone())
                .or_insert_with(|| (wd.value(i).to_string(), src.value(i).to_string()));
            if !rp.value(i).is_empty() {
                have.entry(s).or_insert_with(|| rp.value(i).to_string());
            }
        }
    }

    // The names the memory already resolved, longest first: `funes` → `huggingface/funes`.
    let mut names: Vec<(String, String)> = have
        .values()
        .flat_map(|r| r.split_whitespace())
        .filter_map(|id| id.rsplit_once('/').map(|(_, n)| (n.to_string(), id.to_string())))
        .collect();
    names.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    names.dedup_by(|a, b| a.0 == b.0);

    let mut cwd_cache: HashMap<String, String> = HashMap::new();
    let mut fills = BTreeMap::new();
    let mut final_repos = have.clone();
    let mut unattributed: BTreeMap<String, usize> = BTreeMap::new();

    for (sid, (workdir, source)) in &known {
        if have.contains_key(sid) {
            continue;
        }
        // This machine's checkout, when it still exists — authoritative over any inference.
        let local = match repo::cwd_of_transcript(Path::new(source)) {
            Some(cwd) => cwd_cache
                .entry(cwd.clone())
                .or_insert_with(|| repo::of_cwd(&cwd))
                .clone(),
            None => String::new(),
        };
        let found = if !local.is_empty() {
            Some(local)
        } else if let Some((_, id)) = OVERRIDES.iter().find(|(sfx, _)| workdir.ends_with(sfx)) {
            Some(id.to_string())
        } else {
            names
                .iter()
                .find(|(n, _)| {
                    workdir.ends_with(&format!("-{n}")) || workdir.contains(&format!("-{n}-"))
                })
                .map(|(_, id)| id.clone())
        };
        match found {
            Some(r) => {
                fills.insert(sid.clone(), r.clone());
                final_repos.insert(sid.clone(), r);
            }
            None => *unattributed.entry(workdir.clone()).or_default() += 1,
        }
    }
    Ok(Plan { fills, final_repos, unattributed })
}

fn report(p: &Plan) {
    let mut by_repo: BTreeMap<&str, usize> = BTreeMap::new();
    for r in p.fills.values() {
        *by_repo.entry(r.as_str()).or_default() += 1;
    }
    println!("would fill {} session(s):", p.fills.len());
    for (r, n) in &by_repo {
        println!("  {n:4}  {r}");
    }
    let left: usize = p.unattributed.values().sum();
    println!("\n{left} session(s) stay empty — no checkout to attribute:");
    for (wd, n) in p.unattributed.iter().take(12) {
        println!("  {n:4}  {}", if wd.is_empty() { "<no workdir>" } else { wd });
    }
    if p.unattributed.len() > 12 {
        println!("  … {} more workdir(s)", p.unattributed.len() - 12);
    }
}

fn transform(by_session: HashMap<String, String>) -> NewColumnTransform {
    let out_schema = Arc::new(Schema::new(vec![Field::new("repo", DataType::Utf8, true)]));
    let mapper_schema = out_schema.clone();
    NewColumnTransform::BatchUDF(BatchUDF {
        mapper: Box::new(move |batch: &RecordBatch| {
            let sids = batch
                .column_by_name("session_id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .expect("add_columns read session_id");
            let repos = StringArray::from_iter_values(
                (0..batch.num_rows())
                    .map(|i| by_session.get(sids.value(i)).map(String::as_str).unwrap_or("")),
            );
            RecordBatch::try_new(mapper_schema.clone(), vec![Arc::new(repos)])
                .map_err(lance::Error::from)
        }),
        output_schema: out_schema,
        result_checkpoint: None,
    })
}

fn col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .with_context(|| format!("memory has no `{name}` column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("`{name}` column is not utf8"))
}
