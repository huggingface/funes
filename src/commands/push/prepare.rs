//! Preparation keeps row locations in memory and content on disk. All reads use the same
//! immutable dataset handle: physical row addresses must never be reused against a newer version.

use super::Skipped;
use crate::{chunk::BlockWriter, scan};
use anyhow::{Context, Result};
use arrow_array::{Int64Array, RecordBatch, StringArray, UInt64Array};
use arrow_ipc::{reader::FileReader, writer::FileWriter};
use arrow_schema::SchemaRef;
use futures::TryStreamExt;
use lance::{dataset::ROW_ID, Dataset};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;

/// Bounds fetched content, independently of the number of selected rows or fragments.
const BATCH_ROWS: usize = 8192;

struct Row {
    address: u64,
    block: usize,
    split: i64,
}

pub(super) struct Selection {
    rows: Vec<Row>,
    blocks: Vec<Vec<usize>>,
}

impl Selection {
    /// Never build an ID predicate: Lance would clone that giant expression per fragment.
    pub(super) async fn read(local: &Dataset, ids: &HashSet<String>) -> Result<Self> {
        Self::read_selected(local, Some(ids)).await
    }

    /// An unselected first publication includes every physical row, even one with a null ID.
    pub(super) async fn read_all(local: &Dataset) -> Result<Self> {
        Self::read_selected(local, None).await
    }

    async fn read_selected(local: &Dataset, ids: Option<&HashSet<String>>) -> Result<Self> {
        let mut selected = Self {
            rows: Vec::new(),
            blocks: Vec::new(),
        };
        if ids.is_some_and(HashSet::is_empty) {
            return Ok(selected);
        }
        let mut groups = HashMap::new();
        let mut scan = local.scan();
        scan.project(&["id", "session_id", "turn_uuid", "block_idx", "split_idx"])?;
        scan.with_row_id();
        scan.batch_size(BATCH_ROWS);
        let mut stream = scan.try_into_stream().await?;
        while let Some(batch) = stream.try_next().await? {
            let chunk_ids = strings(&batch, "id")?;
            let sessions = strings(&batch, "session_id")?;
            let turns = strings(&batch, "turn_uuid")?;
            let blocks = batch
                .column_by_name("block_idx")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                .context("selecting push rows: missing block_idx")?;
            let splits = batch
                .column_by_name("split_idx")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                .context("selecting push rows: missing split_idx")?;
            let addresses = batch
                .column_by_name(ROW_ID)
                .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
                .context("selecting push rows: missing row addresses")?;
            for (i, id) in chunk_ids.iter().enumerate() {
                if ids.is_some_and(|ids| !id.is_some_and(|id| ids.contains(id))) {
                    continue;
                }
                let key = (sessions.value(i).to_owned(), turns.value(i).to_owned(), blocks.value(i));
                let block = *groups.entry(key).or_insert_with(|| {
                    selected.blocks.push(Vec::new());
                    selected.blocks.len() - 1
                });
                selected.blocks[block].push(selected.rows.len());
                selected.rows.push(Row {
                    address: addresses.value(i),
                    block,
                    split: splits.value(i),
                });
            }
        }
        // Stable ordering is significant when the same physical chunk occurs more than once.
        for block in &mut selected.blocks {
            block.sort_by_key(|&i| selected.rows[i].split);
        }
        Ok(selected)
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Fetch text across block boundaries in batches, but reconstruct one complete block per file.
    /// The scanner sees exactly the selected splits, as in the original gate, including duplicates.
    async fn stage(&self, local: &Dataset, dir: &std::path::Path) -> Result<()> {
        let text_schema = local.schema().project(&["text"])?;
        let mut ordered = self.blocks.iter().flatten().copied();
        let mut current: Option<(usize, BlockWriter<BufWriter<File>>)> = None;
        loop {
            let indices: Vec<_> = ordered.by_ref().take(BATCH_ROWS).collect();
            if indices.is_empty() {
                break;
            }
            let addresses: Vec<_> = indices.iter().map(|&i| self.rows[i].address).collect();
            let batch = local.take_rows(&addresses, text_schema.clone()).await?;
            anyhow::ensure!(
                batch.num_rows() == indices.len(),
                "selected text rows disappeared from the pinned dataset"
            );
            let texts = strings(&batch, "text")?;
            for (offset, &i) in indices.iter().enumerate() {
                let block = self.rows[i].block;
                if current.as_ref().map(|(b, _)| *b) != Some(block) {
                    if let Some((_, writer)) = current.take() {
                        writer.finish()?;
                    }
                    let file =
                        File::create(dir.join(block.to_string())).context("staging a block for the secret scan")?;
                    current = Some((block, BlockWriter::new(BufWriter::new(file))));
                }
                current.as_mut().unwrap().1.push(texts.value(offset))?;
            }
        }
        if let Some((_, writer)) = current {
            writer.finish()?;
        }
        Ok(())
    }

    pub(super) async fn scan(self, local: &Dataset) -> Result<(CleanRows, Skipped)> {
        let scanner = scan::Trufflehog::find()?;
        let dir = tempfile::tempdir().context("creating the block staging directory")?;
        self.stage(local, dir.path()).await?;
        let findings = scanner.scan_directory(dir.path(), self.blocks.len())?;
        // Remove the staged text before allocating the full-row spool.
        dir.close().context("removing staged secret-scan text")?;
        let mut detectors = Vec::new();
        for found in &findings {
            detectors.extend(scan::detectors(found));
        }
        let addresses: Vec<_> = self
            .rows
            .iter()
            .filter(|row| findings[row.block].is_empty())
            .map(|row| row.address)
            .collect();
        let skipped = Skipped {
            rows: self.rows.len() - addresses.len(),
            summary: scan::summary(detectors.iter().map(String::as_str)),
        };
        Ok((CleanRows { addresses }, skipped))
    }
}

pub(super) struct CleanRows {
    addresses: Vec<u64>,
}

impl CleanRows {
    pub(super) async fn spool(self, local: &Dataset) -> Result<PreparedPush> {
        let schema: SchemaRef = Arc::new(arrow_schema::Schema::from(local.schema()));
        let file = tempfile::NamedTempFile::new().context("creating the clean-row spool")?;
        let mut writer = FileWriter::try_new(BufWriter::new(file.reopen()?), &schema)?;
        let mut ids = HashSet::new();
        for addresses in self.addresses.chunks(BATCH_ROWS) {
            let batch = local.take_rows(addresses, local.schema().clone()).await?;
            anyhow::ensure!(
                batch.num_rows() == addresses.len(),
                "selected rows disappeared from the pinned dataset"
            );
            // Lance scan/take output omits dataset schema metadata, including the model identity.
            let batch = RecordBatch::try_new(schema.clone(), batch.columns().to_vec())?;
            // Match the existing receipt's value-based ID collection, including null-ID rows
            // admitted by an unselected first publication.
            let chunk_ids = strings(&batch, "id")?;
            ids.extend((0..batch.num_rows()).map(|i| chunk_ids.value(i).to_owned()));
            writer.write(&batch).context("writing the clean-row spool")?;
        }
        writer.finish()?;
        std::io::Write::flush(writer.get_mut())?;
        Ok(PreparedPush {
            file,
            schema,
            rows: self.addresses.len(),
            ids,
        })
    }
}

pub(super) struct PreparedPush {
    file: tempfile::NamedTempFile,
    pub(super) schema: SchemaRef,
    pub(super) rows: usize,
    pub(super) ids: HashSet<String>,
}

impl PreparedPush {
    /// Every CAS attempt consumes a new reader of the same successfully prepared payload.
    pub(super) fn reader(&self) -> Result<FileReader<File>> {
        Ok(FileReader::try_new(self.file.reopen()?, None)?)
    }
}

fn strings<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .with_context(|| format!("preparing push: missing or non-string {name}"))
}
