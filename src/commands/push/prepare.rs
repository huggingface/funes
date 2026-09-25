//! Preparation keeps row locations in memory and content on disk. All reads use the same
//! immutable dataset handle: physical row addresses must never be reused against a newer version.

use super::Skipped;
use crate::{chunk::BlockWriter, memory::dataset, scan};
use anyhow::{Context, Result};
use arrow_array::{Array, Int64Array, RecordBatch, StringArray, UInt64Array};
use arrow_ipc::{reader::FileReader, writer::FileWriter};
use arrow_schema::SchemaRef;
use base64::{
    engine::general_purpose::{GeneralPurpose, STANDARD},
    write::EncoderWriter,
};
use futures::TryStreamExt;
use lance::{dataset::ROW_ID, Dataset};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use tokio::task::spawn_blocking;

const BATCH_ROWS: usize = 8192;

struct Row {
    address: u64,
    block: usize,
    split: i64,
}

struct ActiveBlock {
    block: usize,
    writer: BlockWriter<EncoderWriter<'static, GeneralPurpose, BufWriter<File>>>,
}

impl ActiveBlock {
    fn new(block: usize, mut writer: BufWriter<File>) -> Result<Self> {
        write!(writer, "{{\"metadata\":{{\"block\":{block}}},\"data_b64\":\"")?;
        Ok(Self {
            block,
            writer: BlockWriter::new(EncoderWriter::new(writer, &STANDARD)),
        })
    }

    fn finish(self) -> Result<BufWriter<File>> {
        // Finish the base64 record, but keep buffering across block boundaries.
        let mut writer = self.writer.into_inner().finish()?;
        writer.write_all(b"\"}\n")?;
        Ok(writer)
    }
}

pub(super) struct Selection<'a> {
    local: &'a Dataset,
    rows: Vec<Row>,
    blocks: Vec<Vec<usize>>,
}

impl<'a> Selection<'a> {
    /// Never build an ID predicate: Lance would clone that giant expression per fragment.
    pub(super) async fn read(local: &'a Dataset, ids: &HashSet<String>) -> Result<Self> {
        Self::read_selected(local, Some(ids)).await
    }

    /// An unselected first publication includes every physical row, even one with a null ID.
    pub(super) async fn read_all(local: &'a Dataset) -> Result<Self> {
        Self::read_selected(local, None).await
    }

    async fn read_selected(local: &'a Dataset, ids: Option<&HashSet<String>>) -> Result<Self> {
        let mut selected = Self {
            local,
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
            let chunk_ids = column::<StringArray>(&batch, "id")?;
            let sessions = column::<StringArray>(&batch, "session_id")?;
            let turns = column::<StringArray>(&batch, "turn_uuid")?;
            let blocks = column::<Int64Array>(&batch, "block_idx")?;
            let splits = column::<Int64Array>(&batch, "split_idx")?;
            let addresses = column::<UInt64Array>(&batch, ROW_ID)?;
            for (i, id) in chunk_ids.iter().enumerate() {
                let included = match ids {
                    None => true,
                    Some(ids) => id.is_some_and(|id| ids.contains(id)),
                };
                if !included {
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

    /// Fetch text across block boundaries in batches, reconstructing one complete block per record.
    /// The scanner sees exactly the selected splits, including duplicates.
    async fn stage(&self, mut file: tempfile::NamedTempFile) -> Result<tempfile::NamedTempFile> {
        let text_schema = self.local.schema().project(&["text"])?;
        let mut ordered = self.blocks.iter().flatten().copied();
        let mut fetch_next = async || -> Result<_> {
            let indices: Vec<_> = ordered.by_ref().take(BATCH_ROWS).collect();
            if indices.is_empty() {
                return Ok(None);
            }
            let addresses: Vec<_> = indices.iter().map(|&i| self.rows[i].address).collect();
            let batch = self.local.take_rows(&addresses, text_schema.clone()).await?;
            anyhow::ensure!(
                batch.num_rows() == indices.len(),
                "selected text rows disappeared from the pinned dataset"
            );
            let blocks: Vec<_> = indices.iter().map(|&i| self.rows[i].block).collect();
            Ok(Some((blocks, batch)))
        };
        let mut next = fetch_next().await?;
        let mut current: Option<ActiveBlock> = None;
        while let Some((blocks, batch)) = next {
            // One blocking call per fetched batch, with ownership of the file and writer
            // so cancellation cannot remove files while the blocking task is still using them.
            let write = spawn_blocking(move || -> Result<_> {
                let texts = column::<StringArray>(&batch, "text")?;
                for (offset, block) in blocks.into_iter().enumerate() {
                    let mut active = match current.take() {
                        Some(active) if active.block == block => active,
                        previous => {
                            let writer = match previous {
                                Some(previous) => previous.finish()?,
                                None => BufWriter::with_capacity(128 * 1024, file.reopen()?),
                            };
                            ActiveBlock::new(block, writer)?
                        }
                    };
                    active.writer.push(texts.value(offset))?;
                    current = Some(active);
                }
                Ok((file, current))
            });
            // Drive one read alongside the write; join both before propagating either error.
            let (written, fetched) = tokio::join!(write, fetch_next());
            (file, current) = written??;
            next = fetched?;
        }
        spawn_blocking(move || -> Result<_> {
            if let Some(active) = current {
                active.finish()?.flush()?;
            }
            Ok(file)
        })
        .await?
    }

    pub(super) async fn scan(self) -> Result<(CleanRows<'a>, Skipped)> {
        self.scan_with_progress(|_| {}).await
    }

    pub(super) async fn scan_with_progress(self, staged: impl FnOnce(usize)) -> Result<(CleanRows<'a>, Skipped)> {
        let (scanner, file) = spawn_blocking(|| -> Result<_> {
            Ok((
                scan::Trufflehog::find()?,
                tempfile::NamedTempFile::new_in(dataset::staging_root()).context("creating the block staging file")?,
            ))
        })
        .await??;
        let file = self.stage(file).await?;
        let blocks = self.blocks.len();
        staged(blocks);
        let findings = spawn_blocking(move || -> Result<_> {
            let findings = scanner.scan_json(file.path(), blocks)?;
            // Remove the staged text before allocating the full-row spool.
            file.close().context("removing staged secret-scan text")?;
            Ok(findings)
        })
        .await??;
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
        Ok((
            CleanRows {
                local: self.local,
                addresses,
            },
            skipped,
        ))
    }
}

pub(super) struct CleanRows<'a> {
    local: &'a Dataset,
    addresses: Vec<u64>,
}

impl CleanRows<'_> {
    pub(super) async fn spool(self) -> Result<PreparedPush> {
        let schema: SchemaRef = Arc::new(arrow_schema::Schema::from(self.local.schema()));
        let writer_schema = schema.clone();
        let (mut file, mut writer) = spawn_blocking(move || -> Result<_> {
            let file =
                tempfile::NamedTempFile::new_in(dataset::staging_root()).context("creating the clean-row spool")?;
            let writer = FileWriter::try_new(BufWriter::new(file.reopen()?), &writer_schema)?;
            Ok((file, writer))
        })
        .await??;
        let mut ids = HashSet::new();
        let mut chunks = self.addresses.chunks(BATCH_ROWS);
        let mut fetch_next = async || -> Result<_> {
            let Some(addresses) = chunks.next() else {
                return Ok(None);
            };
            let batch = self.local.take_rows(addresses, self.local.schema().clone()).await?;
            anyhow::ensure!(
                batch.num_rows() == addresses.len(),
                "selected rows disappeared from the pinned dataset"
            );
            Ok(Some(batch))
        };
        let mut next = fetch_next().await?;
        while let Some(batch) = next {
            // Lance scan/take output omits dataset schema metadata, including the model identity.
            let batch = RecordBatch::try_new(schema.clone(), batch.columns().to_vec())?;
            let write = spawn_blocking(move || -> Result<_> {
                // Match the existing receipt's value-based ID collection, including null-ID rows
                // admitted by an unselected first publication.
                let chunk_ids = column::<StringArray>(&batch, "id")?;
                ids.extend((0..batch.num_rows()).map(|i| chunk_ids.value(i).to_owned()));
                writer.write(&batch).context("writing the clean-row spool")?;
                Ok((file, writer, ids))
            });
            let (written, fetched) = tokio::join!(write, fetch_next());
            (file, writer, ids) = written??;
            next = fetched?;
        }
        let rows = self.addresses.len();
        spawn_blocking(move || -> Result<_> {
            writer.finish()?;
            writer.get_mut().flush()?;
            Ok(PreparedPush {
                file,
                schema,
                rows,
                ids,
            })
        })
        .await?
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

fn column<'a, T: Array + 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    let array = batch
        .column_by_name(name)
        .with_context(|| format!("preparing push: missing column {name}"))?;
    array.as_any().downcast_ref::<T>().with_context(|| {
        format!(
            "preparing push: unexpected type {} for column {name}",
            array.data_type()
        )
    })
}
