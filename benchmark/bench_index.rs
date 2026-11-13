//! Time the part of `funes index` that writes rows, in a scratch memory:
//!
//! ```text
//! cargo run --release --example bench_index -- --sessions 2000 2>/dev/null
//! cargo run --release --example bench_index -- --dir path/to/turns --limit 5000 2>/dev/null
//! ```
//!
//! An index run writes every session's rows, embeds them, then compacts the memory. The embedding
//! takes the longest and does not depend on how the rows are written, so this leaves it out and
//! times the rest: writing the rows (reading each turns file, chunking it, appending its rows,
//! recording its state), then the end of the run (the language counts and the compaction). Both
//! grow with the number of sessions as well as with their size, so a history of many short sessions
//! is where they show.
//!
//! The corpus is generated, `--sessions` sessions of `--turns` turns that are the same on every run,
//! or a directory of turns files given with `--dir`, of which `--limit` keeps the most recent
//! sessions. The indexer reports each session and the compaction on stderr.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Sessions to generate.
    #[arg(long, default_value_t = 2000, conflicts_with = "dir")]
    sessions: usize,
    /// Turns per generated session.
    #[arg(long, default_value_t = 12, conflicts_with = "dir")]
    turns: usize,
    /// A directory of turns files to index instead of a generated corpus.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Index only the directory's most recent N sessions.
    #[arg(long, requires = "dir")]
    limit: Option<usize>,
}

/// The words a generated turn is drawn from.
const WORDS: [&str; 48] = [
    "the",
    "index",
    "writes",
    "rows",
    "for",
    "each",
    "session",
    "and",
    "then",
    "embeds",
    "them",
    "while",
    "recall",
    "reads",
    "chunks",
    "back",
    "from",
    "a",
    "memory",
    "on",
    "disk",
    "with",
    "its",
    "turns",
    "in",
    "order",
    "so",
    "an",
    "agent",
    "can",
    "find",
    "what",
    "was",
    "decided",
    "before",
    "about",
    "this",
    "repository",
    "build",
    "test",
    "run",
    "error",
    "fix",
    "plan",
    "review",
    "answer",
    "question",
    "user",
];

/// An xorshift generator with a fixed seed, so a generated corpus is the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Write `sessions` turns files of `turns` turns each into `dir`, a turn of 40 to 400 words.
fn generate(dir: &Path, sessions: usize, turns: usize) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for s in 0..sessions {
        let session_id = format!("bench-{s:06}");
        let mut file = String::new();
        for seq in 0..turns {
            let words: Vec<&str> = (0..40 + rng.below(361))
                .map(|_| WORDS[rng.below(WORDS.len())])
                .collect();
            let turn = serde_json::json!({
                "session_id": session_id,
                "turn_uuid": format!("t{seq}"),
                "seq": seq,
                "ts": "2026-01-01T00:00:00Z",
                "role": if seq % 2 == 0 { "user" } else { "assistant" },
                "harness": "bench",
                "blocks": [{"block_type": "text", "text": words.join(" ")}],
            });
            file.push_str(&turn.to_string());
            file.push('\n');
        }
        std::fs::write(dir.join(format!("{session_id}.funes.jsonl")), file)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let scratch = tempfile::tempdir()?;
    let (dir, corpus) = match &args.dir {
        Some(dir) => {
            let kept = args.limit.map(|n| format!(", its {n} most recent sessions"));
            (dir.clone(), format!("{}{}", dir.display(), kept.unwrap_or_default()))
        }
        None => {
            let dir = scratch.path().join("turns");
            generate(&dir, args.sessions, args.turns)?;
            (
                dir,
                format!("{} generated sessions of {} turns", args.sessions, args.turns),
            )
        }
    };
    // A scratch home: the run builds a memory of its own and leaves yours alone.
    std::env::set_var("FUNES_HOME", scratch.path().join("funes"));
    let run = funes::commands::index::write_rows(&dir, args.limit).await?;
    let write = run.write.as_secs_f64();
    println!("\ncorpus: {corpus}");
    println!(
        "rows: {} sessions, {} chunks in {write:.1} s, {:.1} sessions/s",
        run.sessions,
        run.chunks,
        run.sessions as f64 / write
    );
    println!("left: {} fragments to merge", run.fragments);
    println!("end of run: {:.1} s", run.finalize.as_secs_f64());
    Ok(())
}
