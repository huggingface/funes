//! Render blocks to text and split into chunks.
//! All indexing is by Unicode code point (char), not bytes.

use crate::parse::Turn;
use sha1::{Digest, Sha1};

const MAX_CHARS: usize = 1200;
const OVERLAP: usize = 150;

pub struct Chunk {
    pub id: String,
    pub text: String,
    pub session_id: String,
    pub project: String,
    pub turn_uuid: String,
    pub parent_uuid: Option<String>,
    pub seq: i64,
    pub ts: String,
    pub role: String,
    pub block_type: String,
    pub tool_name: Option<String>,
    pub source_path: String,
    pub block_idx: i64,
    pub split_idx: i64,
}

/// A missing tool name renders as the literal "None".
fn py_opt(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("None")
}

fn render(block_type: &str, text: &str, tool_name: &Option<String>) -> String {
    match block_type {
        "tool_use" => format!("[tool_use {}] {}", py_opt(tool_name), text).trim().to_string(),
        "tool_result" => {
            let label = match tool_name {
                Some(n) => format!("tool_result {n}"),
                None => "tool_result".to_string(),
            };
            format!("[{label}] {text}").trim().to_string()
        }
        _ => text.to_string(),
    }
}

/// Last code-point index of `target` in `chars[start..end)`, or -1 if absent.
fn rfind(chars: &[char], target: char, start: usize, end: usize) -> i64 {
    (start..end)
        .rev()
        .find(|&i| chars[i] == target)
        .map(|i| i as i64)
        .unwrap_or(-1)
}

fn split(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.trim().chars().collect();
    let n = chars.len();
    if n == 0 {
        return vec![];
    }
    if n <= MAX_CHARS {
        return vec![chars.iter().collect()];
    }
    let half = (MAX_CHARS / 2) as i64;
    let mut pieces = Vec::new();
    let mut start = 0usize;
    while start < n {
        let mut end = (start + MAX_CHARS).min(n);
        if end < n {
            let mut brk = rfind(&chars, '\n', start, end);
            if brk <= start as i64 + half {
                brk = rfind(&chars, ' ', start, end);
            }
            if brk > start as i64 + half {
                end = brk as usize;
            }
        }
        let piece: String = chars[start..end].iter().collect::<String>().trim().to_string();
        if !piece.is_empty() {
            pieces.push(piece);
        }
        if end >= n {
            break;
        }
        start = end.saturating_sub(OVERLAP).max(start + 1);
    }
    pieces
}

fn cid(session_id: &str, turn_uuid: &str, block_idx: i64, split_idx: i64) -> String {
    let raw = format!("{session_id}:{turn_uuid}:{block_idx}:{split_idx}");
    let mut h = Sha1::new();
    h.update(raw.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

pub fn chunks_from_turns(turns: &[Turn], include_thinking: bool) -> Vec<Chunk> {
    let mut out = Vec::new();
    for turn in turns {
        for (bi, block) in turn.blocks.iter().enumerate() {
            // block_idx counts every block; dropping a thinking block does not renumber it.
            if block.block_type == "thinking" && !include_thinking {
                continue;
            }
            let rendered = render(&block.block_type, &block.text, &block.tool_name);
            for (si, piece) in split(&rendered).into_iter().enumerate() {
                out.push(Chunk {
                    id: cid(&turn.session_id, &turn.turn_uuid, bi as i64, si as i64),
                    text: piece,
                    session_id: turn.session_id.clone(),
                    project: turn.project.clone(),
                    turn_uuid: turn.turn_uuid.clone(),
                    parent_uuid: turn.parent_uuid.clone(),
                    seq: turn.seq,
                    ts: turn.ts.clone(),
                    role: turn.role.clone(),
                    block_type: block.block_type.clone(),
                    tool_name: block.tool_name.clone(),
                    source_path: turn.source_path.clone(),
                    block_idx: bi as i64,
                    split_idx: si as i64,
                });
            }
        }
    }
    out
}
