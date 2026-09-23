//! Dev-only: dump a native transcript through its in-tree parser as `.funes.jsonl` lines, exactly
//! as the transcript-tree source would parse it. Usage: dump_turns <claude> <file>
use funes::traces::{claude, jsonl};
use std::path::Path;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: dump_turns <claude> <file>");
        std::process::exit(2);
    }
    let p = Path::new(&args[2]);
    let (sid, fallback) = (jsonl::session_id_of(p), claude::workdir_of(p));
    let turns = match args[1].as_str() {
        "claude" => claude::turns_from_jsonl_file(p, &sid, &fallback),
        other => {
            eprintln!("unknown harness {other}");
            std::process::exit(2);
        }
    }
    .expect("parse");
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::new(out.lock());
    for t in &turns {
        use std::io::Write;
        writeln!(out, "{}", serde_json::to_string(t).unwrap()).unwrap();
    }
}
