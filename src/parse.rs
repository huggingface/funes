//! Parse Claude Code session transcripts (`*.jsonl`) into turns + blocks.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Block {
    pub block_type: String, // "text" | "thinking" | "tool_use" | "tool_result"
    pub text: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
}

pub struct Turn {
    pub session_id: String,
    pub project: String,
    pub turn_uuid: String,
    pub parent_uuid: Option<String>,
    pub seq: i64,
    pub ts: String,
    pub role: String,
    pub blocks: Vec<Block>,
    pub source_path: String,
}

/// All `*.jsonl` under `root`, recursively, sorted by path.
pub fn iter_jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();
    files
}

pub fn session_id_of(p: &Path) -> String {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
}

/// The path segment right after a `projects` dir, else the parent dir name.
pub fn project_of(p: &Path) -> String {
    let parts: Vec<&str> = p.iter().filter_map(|s| s.to_str()).collect();
    if let Some(i) = parts.iter().position(|&s| s == "projects") {
        if i + 1 < parts.len() {
            return parts[i + 1].to_string();
        }
    }
    p.parent()
        .and_then(|d| d.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

fn flatten_tool_result(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut parts = Vec::new();
        for c in arr {
            if let Some(obj) = c.as_object() {
                if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                    parts.push(obj.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string());
                }
            } else if let Some(s) = c.as_str() {
                parts.push(s.to_string());
            }
        }
        return parts.join("\n");
    }
    String::new()
}

fn normalize_blocks(content: &Value) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(s) = content.as_str() {
        if !s.trim().is_empty() {
            out.push(Block {
                block_type: "text".into(),
                text: s.to_string(),
                tool_name: None,
                tool_use_id: None,
            });
        }
        return out;
    }
    let arr = match content.as_array() {
        Some(a) => a,
        None => return out,
    };
    for b in arr {
        let obj = match b.as_object() {
            Some(o) => o,
            None => continue,
        };
        match obj.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => {
                let t = obj.get("text").and_then(|x| x.as_str()).unwrap_or("");
                if !t.trim().is_empty() {
                    out.push(Block {
                        block_type: "text".into(),
                        text: t.to_string(),
                        tool_name: None,
                        tool_use_id: None,
                    });
                }
            }
            "thinking" => {
                let t = obj.get("thinking").and_then(|x| x.as_str()).unwrap_or("");
                if !t.trim().is_empty() {
                    out.push(Block {
                        block_type: "thinking".into(),
                        text: t.to_string(),
                        tool_name: None,
                        tool_use_id: None,
                    });
                }
            }
            "tool_use" => {
                let input = obj
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()));
                // Compact JSON, literal UTF-8, keys in source order.
                let text = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                let name = obj.get("name").and_then(|x| x.as_str()).map(str::to_string);
                let id = obj.get("id").and_then(|x| x.as_str()).map(str::to_string);
                out.push(Block {
                    block_type: "tool_use".into(),
                    text,
                    tool_name: name,
                    tool_use_id: id,
                });
            }
            "tool_result" => {
                let content = obj.get("content").cloned().unwrap_or(Value::Null);
                let text = flatten_tool_result(&content);
                if !text.trim().is_empty() {
                    let id = obj.get("tool_use_id").and_then(|x| x.as_str()).map(str::to_string);
                    out.push(Block {
                        block_type: "tool_result".into(),
                        text,
                        tool_name: None,
                        tool_use_id: id,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

pub fn turns_from_jsonl_file(p: &Path, session_id: &str, project: &str) -> std::io::Result<Vec<Turn>> {
    let raw = std::fs::read(p)?;
    let content = String::from_utf8_lossy(&raw);
    let mut records: Vec<Value> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            records.push(v);
        }
    }

    let mut turns = Vec::new();
    let mut seq = 0i64; // index among RETAINED turns, file order
    for rec in &records {
        let obj = match rec.as_object() {
            Some(o) => o,
            None => continue,
        };
        let rtype = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if rtype != "user" && rtype != "assistant" {
            continue;
        }
        let msg = obj.get("message").and_then(|m| m.as_object());
        let content = msg.and_then(|m| m.get("content")).cloned().unwrap_or(Value::Null);
        let blocks = normalize_blocks(&content);
        if blocks.is_empty() {
            continue;
        }
        let role = msg
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| rtype.to_string());
        turns.push(Turn {
            session_id: session_id.to_string(),
            project: project.to_string(),
            turn_uuid: obj.get("uuid").and_then(|u| u.as_str()).unwrap_or("").to_string(),
            parent_uuid: obj.get("parentUuid").and_then(|u| u.as_str()).map(str::to_string),
            seq,
            ts: obj.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            role,
            blocks,
            source_path: p.to_string_lossy().into_owned(),
        });
        seq += 1;
    }

    // tool_use_id -> tool_name correlation, scoped to this file (last write wins)
    let mut tool_by_id: HashMap<String, Option<String>> = HashMap::new();
    for t in &turns {
        for b in &t.blocks {
            if b.block_type == "tool_use" {
                if let Some(id) = &b.tool_use_id {
                    tool_by_id.insert(id.clone(), b.tool_name.clone());
                }
            }
        }
    }
    for t in &mut turns {
        for b in &mut t.blocks {
            if b.block_type == "tool_result" && b.tool_name.is_none() {
                if let Some(id) = &b.tool_use_id {
                    if let Some(name) = tool_by_id.get(id) {
                        b.tool_name = name.clone();
                    }
                }
            }
        }
    }
    Ok(turns)
}
