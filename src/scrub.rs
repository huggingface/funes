//! The `scrub` command: redact secrets from the existing local store, in place. Works on the stored
//! rows themselves — so it cleans sessions whose source transcripts are already gone, which
//! re-indexing cannot. Operates only on the local store; it does not touch a published remote.

use crate::index::{build_batch, embed_batched, schema};
use crate::{chunk, dataset, scan};
use anyhow::Result;
use arrow_array::{BooleanArray, RecordBatch, RecordBatchIterator};
use arrow_select::filter::filter_record_batch;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use lance::dataset::{Dataset, WriteMode, WriteParams};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Reconstruct each block, scan them all in one pass, and for any block holding a secret either
/// redact the matching values in place (re-chunked, re-embedded) or — when a value can't be matched
/// (e.g. a key stored with escaped `\n`) — drop the block whole. Fail-closed on the scanner:
/// scrubbing is the whole point.
pub async fn run() -> Result<()> {
    let uri = dataset::table_uri(&dataset::local_store_dir());
    let Ok(ds) = dataset::open(&uri, HashMap::new()).await else {
        println!("no local index to scrub");
        return Ok(());
    };
    let scanner = scan::Trufflehog::find()?;

    let batches = dataset::scan_rows(&ds, &[], None, None).await?;
    let chunks = chunk::chunks_from_batches(&batches);
    let total = chunks.len();
    if total == 0 {
        println!("store is empty");
        return Ok(());
    }

    // Reconstruct each block's contiguous text (so a secret split() cut across chunks is whole) and
    // scan them all in one pass.
    let blocks = chunk::reconstruct_blocks(&chunks);
    let texts: Vec<&str> = blocks.iter().map(|(_, text)| text.as_str()).collect();
    let found = scan::scan_blocks(&texts, &scanner)?;

    // From that single scan, decide each block: excise the secrets whose value matches and re-chunk
    // the result; if any can't be matched, the block can't be safely redacted, so drop it whole.
    // `remove[i]` marks an original row to drop (its block had a secret); `replacements` are the
    // re-chunked redacted blocks to re-embed.
    let mut remove = vec![false; chunks.len()];
    let mut replacements: Vec<chunk::Chunk> = Vec::new();
    let mut redacted_detectors: Vec<String> = Vec::new();
    let mut dropped_detectors: Vec<String> = Vec::new();
    let (mut redacted_blocks, mut dropped_blocks, mut dropped_rows) = (0usize, 0usize, 0usize);
    for ((idxs, text), findings) in blocks.iter().zip(&found) {
        if findings.is_empty() {
            continue;
        }
        for &i in idxs {
            remove[i] = true;
        }
        let r = scan::excise(text, findings);
        if r.fully_redacted {
            replacements.extend(chunk::resplit(&chunks[idxs[0]], &r.text));
            redacted_detectors.extend(r.removed_detectors);
            redacted_blocks += 1;
        } else {
            dropped_detectors.extend(scan::detectors(findings));
            dropped_blocks += 1;
            dropped_rows += idxs.len();
        }
    }
    if !remove.iter().any(|&r| r) {
        println!("store is already clean ({total} chunks)");
        return Ok(());
    }

    // Re-embed only the redacted replacement rows — their stored vectors were computed from the
    // secret-bearing text. Clean rows keep their existing vectors (carried below).
    let replacement_batch = if replacements.is_empty() {
        None
    } else {
        let mut embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
        let rtexts: Vec<&str> = replacements.iter().map(|c| c.text.as_str()).collect();
        let vectors = embed_batched(&mut embedder, &rtexts, |_| {})?;
        Some(build_batch(&replacements, &vectors)?)
    };

    // Rewrite the store in a single Overwrite commit: every clean row (with its existing vector) plus
    // the re-chunked redacted blocks. One commit is deliberate — a delete-then-append is two commits,
    // and an interrupt between them would drop the secret rows without writing their replacements,
    // which for a source-gone session is permanent loss. Append-first isn't a safe alternative either:
    // `cid` hashes only coordinates, so a same-piece-count re-split reuses the old ids and a later
    // delete couldn't tell the fresh rows from the stale ones. (Cost: scrub rewrites the whole table,
    // fine for a rare remediation.)
    let schema = schema();
    let mut out: Vec<RecordBatch> = Vec::new();
    let mut base = 0usize;
    for b in &batches {
        let mask: BooleanArray = (0..b.num_rows()).map(|i| !remove[base + i]).collect();
        let kept = filter_record_batch(b, &mask)?;
        if kept.num_rows() > 0 {
            out.push(RecordBatch::try_new(schema.clone(), kept.columns().to_vec())?);
        }
        base += b.num_rows();
    }
    out.extend(replacement_batch);
    let reader = RecordBatchIterator::new(out.into_iter().map(Ok), schema.clone());
    let mut ds = Dataset::write(
        reader,
        &uri,
        Some(WriteParams {
            mode: WriteMode::Overwrite,
            ..Default::default()
        }),
    )
    .await?;
    dataset::build_indexes(&mut ds).await;

    let mut msg = format!(
        "scrubbed {total} rows: redacted {} secret(s) in {redacted_blocks} block(s)",
        redacted_detectors.len()
    );
    if dropped_blocks > 0 {
        let _ = write!(
            msg,
            "; dropped {dropped_rows} row(s) in {dropped_blocks} block(s) that couldn't be safely redacted ({})",
            scan::summary(dropped_detectors.iter().map(String::as_str))
        );
    }
    println!("{msg}");
    Ok(())
}
