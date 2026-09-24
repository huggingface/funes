#!/usr/bin/env node
// Convert a pi native session (`~/.pi/agent/sessions/<munged-cwd>/<stem>.jsonl`) into a funes
// turns file (`<stem>.funes.jsonl`, docs/funes-jsonl.md).
//
// Usage: convert.mjs <session.jsonl | sessions-root> <spool-dir>
//
// Also a module — `convert` for one session, `convertTree` for a whole store — so a session
// converted mid-run and one converted in bulk go through the same mapping. It uses nothing but the
// JS runtime pi already provides.
//
// `session_id` is the session file's stem, not the id on its `session` line: the stem is what funes
// keyed pi's sessions by while it parsed them itself, and changing it would re-key every session a
// memory already holds.

import { readFileSync, writeFileSync, renameSync, mkdirSync, readdirSync, statSync, realpathSync, utimesSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

const HARNESS = "pi";
const FORMAT = 1;

// Unicode White_Space, which is what Rust's `str::trim` tests. JS's own `trim` differs on two code
// points — it trims U+FEFF and does not trim U+0085 — and a block is kept or dropped on this test.
const WS = "\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000";
const BLANK = new RegExp(`^[${WS}]*$`);
const TRIM = new RegExp(`^[${WS}]+|[${WS}]+$`, "g");
const isBlank = (s) => BLANK.test(s);
const rustTrim = (s) => s.replace(TRIM, "");

// An unpaired surrogate, which only a `\uD800`-`\uDFFF` escape can introduce. serde_json rejects
// such a line outright and funes drops it; JSON.parse accepts it.
const LONE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/;
const MAYBE_LONE = /\\u[dD]/;

function hasLoneSurrogate(v) {
  if (typeof v === "string") return LONE.test(v);
  if (Array.isArray(v)) return v.some(hasLoneSurrogate);
  if (v && typeof v === "object") {
    return Object.keys(v).some((k) => LONE.test(k) || hasLoneSurrogate(v[k]));
  }
  return false;
}

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);
// `Value::as_str`: a string, else nothing.
const str = (v) => (typeof v === "string" ? v : undefined);

/** Every parseable JSON object of a JSONL file, in file order (`jsonl::read_jsonl_records`). */
function readRecords(path) {
  const records = [];
  for (let line of readFileSync(path, "utf8").split("\n")) {
    if (line.endsWith("\r")) line = line.slice(0, -1); // Rust's `str::lines`
    line = rustTrim(line);
    if (line === "") continue;
    let value;
    try {
      value = JSON.parse(line);
    } catch {
      continue; // a partial trailing write
    }
    if (MAYBE_LONE.test(line) && hasLoneSurrogate(value)) continue;
    records.push(value);
  }
  return records;
}

/** A `text`/`thinking` block, or nothing when the text is blank. */
function textLike(blockType, text) {
  return isBlank(text) ? undefined : { block_type: blockType, text };
}

/** A `toolCall` part as a tool_use block; `arguments` compacted, a missing one as `{}`. */
function toolUseBlock(part) {
  const block = {
    block_type: "tool_use",
    text: JSON.stringify("arguments" in part ? part.arguments : {}),
  };
  const name = str(part.name);
  if (name !== undefined) block.tool_name = name;
  const id = str(part.id);
  if (id !== undefined) block.tool_use_id = id;
  return block;
}

/** A tool result's `content` — a string, or a list of `{type:"text",text}` parts — as one string. */
function flattenToolResult(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  const parts = [];
  for (const c of content) {
    if (isObject(c)) {
      if (c.type === "text") parts.push(str(c.text) ?? "");
    } else if (typeof c === "string") {
      parts.push(c);
    }
  }
  return parts.join("\n");
}

function blocksOf(role, msg) {
  const content = Array.isArray(msg.content) ? msg.content : undefined;
  if (role === "user") {
    if (!content) return [];
    const blocks = [];
    for (const part of content) {
      if (!isObject(part) || part.type !== "text") continue;
      const b = textLike("text", str(part.text) ?? "");
      if (b) blocks.push(b);
    }
    return blocks;
  }
  if (role === "assistant") {
    if (!content) return [];
    const blocks = [];
    for (const part of content) {
      if (!isObject(part)) continue;
      if (part.type === "thinking") {
        const b = textLike("thinking", str(part.thinking) ?? "");
        if (b) blocks.push(b);
      } else if (part.type === "text") {
        const b = textLike("text", str(part.text) ?? "");
        if (b) blocks.push(b);
      } else if (part.type === "toolCall") {
        blocks.push(toolUseBlock(part));
      }
    }
    return blocks;
  }
  if (role === "toolResult") {
    const text = flattenToolResult(msg.content);
    if (isBlank(text)) return [];
    const block = { block_type: "tool_result", text };
    const name = str(msg.toolName);
    if (name !== undefined) block.tool_name = name;
    const id = str(msg.toolCallId);
    if (id !== undefined) block.tool_use_id = id;
    return [block];
  }
  return [];
}

/**
 * A record's `timestamp` as the format wants it. pi writes an ISO string; a pre-v2 session wrote
 * epoch milliseconds, which `.funes.jsonl` cannot carry as a number of digits — that line would
 * reject the whole file — so it is rendered as the instant it names.
 */
function timestamp(value) {
  if (typeof value === "string") return value;
  if (typeof value === "number" && Number.isFinite(value)) return new Date(value).toISOString();
  return "";
}

export function turnsOf(sessionPath, sessionId) {
  const records = readRecords(sessionPath);
  const header = records.find((r) => isObject(r) && r.type === "session");
  const cwd = header ? str(header.cwd) : undefined;

  const turns = [];
  let seq = 0;
  for (const record of records) {
    if (!isObject(record) || record.type !== "message") continue;
    const msg = record.message;
    if (!isObject(msg)) continue;
    const nativeRole = str(msg.role) ?? "";
    const blocks = blocksOf(nativeRole, msg);
    if (blocks.length === 0) continue;

    // Field order is the wire order of funes's own Turn.
    const turn = { format: FORMAT, session_id: sessionId };
    if (cwd !== undefined) turn.cwd = cwd;
    turn.turn_uuid = str(record.id) ?? "";
    const parent = str(record.parentId);
    if (parent !== undefined) turn.parent_uuid = parent;
    turn.seq = seq++;
    turn.ts = timestamp(record.timestamp);
    turn.role = nativeRole === "toolResult" ? "tool" : nativeRole;
    turn.blocks = blocks;
    turn.harness = HARNESS;
    turns.push(turn);
  }
  return turns;
}

/** One session into `<spool>/<stem>.funes.jsonl`; returns the path written. */
export function convert(sessionPath, outArg) {
  const stem = basename(sessionPath).replace(/\.jsonl$/, "");
  const out = outArg.endsWith(".funes.jsonl") ? outArg : join(outArg, `${stem}.funes.jsonl`);
  const body = turnsOf(sessionPath, stem)
    .map((t) => JSON.stringify(t) + "\n")
    .join("");
  // Atomic, and the temporary name is not a `.jsonl` one, so a concurrent `funes index` over the
  // spool never lists it.
  const tmp = `${out}.tmp${process.pid}`;
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(tmp, body);
  // Stamped with the session's own time, so funes drains the spool newest session first rather
  // than in the order the seed happened to convert them.
  const { atime, mtime } = statSync(sessionPath);
  utimesSync(tmp, atime, mtime);
  renameSync(tmp, out);
  return out;
}

/** Every session under a `~/.pi/agent/sessions` tree into the spool — with `since` (epoch ms), only
 * those written after it, and never `except`. */
export function convertTree(root, spool, since = 0, except = "") {
  const written = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const p = join(root, entry.name);
    if (entry.isDirectory()) written.push(...convertTree(p, spool, since, except));
    else if (entry.name.endsWith(".jsonl") && p !== except && (!since || statSync(p).mtimeMs > since)) {
      written.push(convert(p, spool));
    }
  }
  return written;
}

// `import.meta.url` is the resolved path; argv[1] is not, so a run through a symlinked directory
// (macOS's /tmp) would otherwise miss.
if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) {
  const [src, spool] = process.argv.slice(2);
  if (!src || !spool) {
    console.error("usage: convert.mjs <session.jsonl | sessions-root> <spool-dir>");
    process.exit(2);
  }
  const written = statSync(src).isDirectory() ? convertTree(src, spool) : [convert(src, spool)];
  console.log(written.length);
}
