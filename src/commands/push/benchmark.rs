//! Opt-in workload measurements, kept outside the production push API.

use super::*;
use arrow_array::{Array, FixedSizeListArray, Float32Array, RecordBatchIterator};
use lance::dataset::WriteParams;
use sha2::{Digest, Sha256};

/// Hash logical values with null tags, independently of record-batch boundaries.
#[derive(Default)]
struct Fingerprint {
    hashes: [Sha256; 3],
    rows: usize,
}

impl Fingerprint {
    fn update(&mut self, batch: &RecordBatch) {
        self.rows += batch.num_rows();
        for (col, name) in ["id", "text"].iter().enumerate() {
            let values = batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for value in values.iter() {
                self.hashes[col].update([u8::from(value.is_some())]);
                if let Some(value) = value {
                    self.hashes[col].update((value.len() as u64).to_le_bytes());
                    self.hashes[col].update(value.as_bytes());
                }
            }
        }
        let vectors = batch
            .column_by_name("vector")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        for i in 0..vectors.len() {
            self.hashes[2].update([u8::from(vectors.is_valid(i))]);
            if vectors.is_null(i) {
                continue;
            }
            self.hashes[2].update(vectors.value_length().to_le_bytes());
            let vector = vectors.value(i);
            let values = vector.as_any().downcast_ref::<Float32Array>().unwrap();
            for value in values.iter() {
                self.hashes[2].update([u8::from(value.is_some())]);
                if let Some(value) = value {
                    self.hashes[2].update(value.to_bits().to_le_bytes());
                }
            }
        }
    }

    fn digests(self) -> Vec<String> {
        self.hashes
            .into_iter()
            .map(|hash| hex::encode(hash.finalize()))
            .collect()
    }
}

/// Measures filtered append preparation. First-publication/null-ID behavior has separate tests.
/// The runtime matches the CLI's default multithread Tokio runtime.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "explicit local dataset and process memory guard required"]
async fn benchmark_push_preparation() {
    let uri = std::env::var("FUNES_BENCH_DATASET").expect("FUNES_BENCH_DATASET");
    assert!(
        Path::new(&uri).is_absolute(),
        "benchmark requires an absolute local fixture path"
    );
    let mode = std::env::var("FUNES_BENCH_MODE").expect("FUNES_BENCH_MODE: legacy or staged");
    assert!(matches!(mode.as_str(), "legacy" | "staged"));
    scan::Trufflehog::find().expect("benchmark requires the production secret scanner");
    let start = std::time::Instant::now();
    let ds = dataset::open(&uri, HashMap::new()).await.unwrap();
    eprintln!(
        "BENCH opened version={} rows={} fragments={} seconds={:.3}",
        ds.version().version,
        ds.count_rows(None).await.unwrap(),
        ds.get_fragments().len(),
        start.elapsed().as_secs_f64()
    );
    let ids = all_ids(&ds).await.unwrap();
    eprintln!("BENCH selection seconds={:.3}", start.elapsed().as_secs_f64());
    let mut fingerprint = Fingerprint::default();
    let held;
    if mode == "legacy" {
        let batches = rows_with_ids(&ds, &ids).await.unwrap();
        eprintln!("BENCH secret_gate seconds={:.3}", start.elapsed().as_secs_f64());
        let (batches, skipped) = drop_secret_rows(batches).unwrap();
        held = skipped.rows;
        eprintln!("BENCH fingerprint seconds={:.3}", start.elapsed().as_secs_f64());
        for batch in batches {
            fingerprint.update(&batch);
        }
    } else {
        let selection = prepare::Selection::read(&ds, &ids).await.unwrap();
        drop(ids);
        eprintln!("BENCH secret_gate seconds={:.3}", start.elapsed().as_secs_f64());
        // Includes text staging, scanner execution, parsing and temporary-file cleanup.
        let (clean, skipped) = selection.scan(&ds).await.unwrap();
        held = skipped.rows;
        eprintln!("BENCH spool seconds={:.3}", start.elapsed().as_secs_f64());
        let prepared = clean.spool(&ds).await.unwrap();
        assert_eq!(prepared.reader().unwrap().schema(), prepared.schema);
        eprintln!("BENCH fingerprint seconds={:.3}", start.elapsed().as_secs_f64());
        for batch in prepared.reader().unwrap() {
            fingerprint.update(&batch.unwrap());
        }
        if std::env::var_os("FUNES_BENCH_CAPTURE").is_some() {
            eprintln!("BENCH capture_append seconds={:.3}", start.elapsed().as_secs_f64());
            let (written, bytes) = remote::benchmark_append(prepared.reader().unwrap()).await.unwrap();
            assert_eq!(written, prepared.rows);
            eprintln!(
                "BENCH capture_complete bytes={bytes} seconds={:.3}",
                start.elapsed().as_secs_f64()
            );
        }
    }
    let rows = fingerprint.rows;
    let fingerprints = fingerprint.digests();
    eprintln!(
        "BENCH complete mode={mode} rows={rows} held={held} digests={fingerprints:?} seconds={:.3}",
        start.elapsed().as_secs_f64()
    );
}

/// Fixed-seed fixture generation is a separate invocation and never part of measured preparation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "explicit new local fixture path required"]
async fn generate_push_fixture() {
    let path = PathBuf::from(std::env::var("FUNES_BENCH_DATASET").expect("FUNES_BENCH_DATASET"));
    assert!(
        path.is_absolute() && !path.exists(),
        "fixture must be a new absolute local path"
    );
    let metadata_path = path.with_extension("fixture.json");
    assert!(!metadata_path.exists(), "fixture metadata already exists");
    let number = |name: &str, default: usize| {
        std::env::var(name)
            .map(|s| s.parse::<usize>().expect(name))
            .unwrap_or(default)
    };
    let rows = number("FUNES_BENCH_ROWS", 10_000);
    let text_bytes = number("FUNES_BENCH_TEXT_BYTES", 2048);
    let splits = number("FUNES_BENCH_SPLITS", 8);
    let rows_per_fragment = number("FUNES_BENCH_ROWS_PER_FRAGMENT", 1000);
    assert!(rows > 0 && splits > 0 && rows_per_fragment > 0);
    assert!(
        (32..=1_048_576).contains(&text_bytes),
        "text bytes must be 32..=1048576"
    );
    let schema = dataset::schema();
    // At most 256 rows and roughly 4 MiB of generated text per input batch. Lance's writer
    // applies its own buffering; fixture construction is explicitly outside the measurement.
    let batch_rows = 256.min((4 * 1024 * 1024 / text_bytes).max(1)).min(rows_per_fragment);
    let batches = (0..rows).step_by(batch_rows).map(move |start| {
        synthetic_batch(start, (rows - start).min(batch_rows), text_bytes, splits)
            .map_err(|error| arrow_schema::ArrowError::ExternalError(error.into()))
    });
    let reader = RecordBatchIterator::new(batches, schema);
    let ds = Dataset::write(
        reader,
        path.to_str().unwrap(),
        Some(WriteParams {
            max_rows_per_file: rows_per_fragment,
            max_rows_per_group: batch_rows,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(ds.count_rows(None).await.unwrap(), rows);
    let manifest = serde_json::json!({
        "generator": "funes-push-synthetic-v1", "seed": 1, "dataset": path,
        "version": ds.version().version, "rows": rows, "text_bytes_per_row": text_bytes,
        "text_bytes": rows as u64 * text_bytes as u64, "vector_dimensions": dataset::DIM,
        "vector_bytes": rows as u64 * dataset::DIM as u64 * 4,
        "splits_per_block": splits, "blocks": rows.div_ceil(splits),
        "rows_per_fragment_requested": rows_per_fragment, "fragments": ds.get_fragments().len(),
        "generation_batch_rows": batch_rows,
        "notes": "Synthetic clean prose and finite pseudorandom vectors; no inference, network, or private transcripts. Wide text is a stress case, not representative chunking."
    });
    std::fs::write(&metadata_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    eprintln!("fixture metadata: {}", metadata_path.display());
}

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn synthetic_batch(start: usize, rows: usize, text_bytes: usize, splits: usize) -> Result<RecordBatch> {
    let mut chunks = Vec::with_capacity(rows);
    let mut vectors = Vec::with_capacity(rows);
    for row in start..start + rows {
        let block = row / splits;
        let mut state = (row as u64).wrapping_add(1);
        let words = [
            "parser", "window", "forest", "table", "number", "violet", "buffer", "orbit",
        ];
        let mut text = format!("synthetic row {row}: ");
        while text.len() < text_bytes {
            text.push_str(words[next_random(&mut state) as usize % words.len()]);
            text.push(' ');
        }
        text.truncate(text_bytes); // Generated text is ASCII; byte truncation cannot split a character.
        chunks.push(chunk::Chunk {
            id: format!("{row:016x}"),
            text,
            session_id: format!("synthetic-{}", block / 100),
            workdir: "synthetic".into(),
            turn_uuid: format!("block-{block}"),
            parent_uuid: None,
            seq: block as i64,
            ts: "2026-01-01T00:00:00Z".into(),
            role: "assistant".into(),
            block_type: "text".into(),
            tool_name: None,
            source_path: "synthetic.jsonl".into(),
            block_idx: 0,
            split_idx: (row % splits) as i64,
            harness: "codex".into(),
            repo: String::new(),
        });
        // Keep vector contents identical when varying only the text-width axis.
        state = (row as u64).wrapping_add(1);
        vectors.push(
            (0..dataset::DIM)
                .map(|_| ((next_random(&mut state) >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0)
                .collect(),
        );
    }
    dataset::build_batch(&chunks, &vectors)
}

#[test]
fn synthetic_fixture_and_fingerprint_are_batch_independent() {
    let whole = synthetic_batch(0, 5, 64, 2).unwrap();
    let batches = [
        synthetic_batch(0, 2, 64, 2).unwrap(),
        synthetic_batch(2, 3, 64, 2).unwrap(),
    ];
    let wide = synthetic_batch(0, 5, 128, 2).unwrap();
    assert_eq!(whole.column_by_name("vector"), wide.column_by_name("vector"));
    let reconstructed = arrow_select::concat::concat_batches(&whole.schema(), &batches).unwrap();
    assert_eq!(whole, reconstructed);
    let mut one = Fingerprint::default();
    one.update(&whole);
    let mut many = Fingerprint::default();
    for batch in &batches {
        many.update(batch);
    }
    assert_eq!(one.digests(), many.digests());
    let mut empty = Fingerprint::default();
    empty.update(&RecordBatch::new_empty(whole.schema()));
    assert_eq!(empty.rows, 0);
}

#[test]
fn fingerprint_distinguishes_null_from_empty_and_vector_nulls() {
    use arrow_array::types::Float32Type;
    let base = synthetic_batch(0, 1, 64, 1).unwrap();
    let digest = |id, value: Option<f32>| {
        let mut columns = base.columns().to_vec();
        columns[0] = Arc::new(StringArray::from(vec![id]));
        columns[14] = Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            [Some(vec![value; dataset::DIM as usize])],
            dataset::DIM,
        ));
        let batch = RecordBatch::try_new(base.schema(), columns).unwrap();
        let mut fingerprint = Fingerprint::default();
        fingerprint.update(&batch);
        fingerprint.digests()
    };
    assert_ne!(digest(None, Some(0.0)), digest(Some(""), Some(0.0)));
    assert_ne!(digest(Some("id"), None), digest(Some("id"), Some(0.0)));
}
