// The extension, driven as pi drives it. At startup, a history `setup` could not convert (no JS
// runtime on PATH) is converted before the worker that indexes it is launched — the publish worker
// with a memory bound, which indexes first, and the index worker without — and a `funes` that dies
// as the session starts costs recall and nothing else. Per turn, the session in progress is
// converted, then the sessions the last sweep missed, under the root `setup` found and nowhere
// else. Loads index.ts with a fake pi and a fake `funes mcp`, and reads what the worker saw in the
// spool when it started.
//
//   node integrations/pi/test/startup.mjs    (run.sh runs it)
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
delete process.env.FUNES_MEMORY;

// Only `sh` is promised on a box running pi: a `bash` that fails proves nothing is spawned
// through it.
const shims = mkdtempSync(join(tmpdir(), "funes-pi-shims-"));
writeFileSync(join(shims, "bash"), "#!/bin/sh\nexit 127\n");
chmodSync(join(shims, "bash"), 0o755);
process.env.PATH = `${shims}:${process.env.PATH}`;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// A session written now, under `dir`: `cp` would keep the fixture's stamp, and the sweep goes by
// stamps.
function session(dir, name) {
  mkdirSync(dir, { recursive: true });
  const file = join(dir, name);
  writeFileSync(file, readFileSync(join(HERE, "session.jsonl")));
  return file;
}

// The extension as installed under `root`: its files, the records `setup` writes beside them, and
// a worker script that records what it was launched for and what the spool held when it started.
function install(root, { memory = "", pending = false } = {}) {
  const ext = join(root, "ext");
  mkdirSync(join(ext, "scripts"), { recursive: true });
  for (const name of ["index.ts", "convert.mjs"]) cpSync(join(HERE, "..", name), join(ext, name));
  const sessions = join(root, "sessions");
  mkdirSync(sessions, { recursive: true });
  const spool = join(root, "spool");
  mkdirSync(spool);
  writeFileSync(join(ext, "spool"), `${spool}\n`);
  writeFileSync(join(ext, "sessions"), `${sessions}\n`);
  if (pending) writeFileSync(join(ext, "seed-pending"), `${sessions}\n`);
  if (memory) writeFileSync(join(ext, "memory"), `${memory}\n`);
  const log = join(root, "worker.log");
  const script = join(ext, "scripts", "funes-index.sh");
  writeFileSync(script, `#!/bin/sh\n{ printf 'funes-index.sh%s\\n' "${"${*:+ $*}"}"; ls "${spool}"; } >"${log}.tmp" && mv "${log}.tmp" "${log}"\n`);
  chmodSync(script, 0o755);
  return { ext, sessions, spool, log };
}

// Load the installed extension with a fake pi, returning its handlers by event.
async function activate(ext) {
  const handlers = {};
  const pi = { registerTool() {}, on(name, fn) { handlers[name] = fn; } };
  const { default: activate } = await import(pathToFileURL(join(ext, "index.ts")).href);
  await activate(pi);
  return handlers;
}

async function workerLog(log, what) {
  const deadline = Date.now() + 10_000;
  while (!existsSync(log)) {
    if (Date.now() > deadline) throw new Error(`no worker ran for ${what}`);
    await sleep(50);
  }
  return readFileSync(log, "utf8");
}

// A `funes` whose `mcp` is `body`: echoing each request back as its reply is a server with no
// tools, enough to load; answering once and exiting is one that dies under the handshake.
function fakeFunes(root, body) {
  const path = join(root, "funes");
  writeFileSync(path, `#!/bin/sh\n${body}\n`);
  chmodSync(path, 0o755);
  return path;
}

async function startup(memory, funes = "exec cat") {
  const root = mkdtempSync(join(tmpdir(), "funes-pi-startup-"));
  process.env.FUNES_BIN = fakeFunes(root, funes);
  const { ext, sessions, log } = install(root, { memory, pending: true });
  // pi's layout: one directory per project under the sessions root.
  session(join(sessions, "--Users-me-repo--"), "session.jsonl");

  const handlers = await activate(ext);
  const notices = [];
  await handlers.session_start({ reason: "startup" }, { ui: { notify: (text) => notices.push(text) } });

  if (existsSync(join(ext, "seed-pending"))) throw new Error("the pending history was not converted");
  return { log: await workerLog(log, memory || "the local memory"), notices };
}

// A turn of the session at `file`, with `stale` written since the last sweep and `foreign` a
// `.jsonl` no sweep may touch: what the spool holds afterwards.
async function turn(root, file, stale, foreign) {
  const { ext, spool, log } = install(root);
  process.env.FUNES_BIN = fakeFunes(root, "exec cat");
  const handlers = await activate(ext);
  // The records are older than the sessions, so the sweep sees the sessions as new.
  await sleep(20);
  const named = session(dirname(file), file.split("/").pop());
  const others = [session(dirname(stale), stale.split("/").pop()), session(dirname(foreign), foreign.split("/").pop())];
  await handlers.turn_end({}, { sessionManager: { getSessionFile: () => named } });
  await workerLog(log, "the turn");
  const converted = (p) => existsSync(join(spool, `${p.split("/").pop().replace(/\.jsonl$/, "")}.funes.jsonl`));
  return { named: converted(named), stale: converted(others[0]), foreign: converted(others[1]), swept: existsSync(join(ext, "swept")) };
}

const local = await startup("");
if (local.log !== "funes-index.sh\nsession.funes.jsonl\n") {
  throw new Error(`local memory: expected the index worker to find the seeded session, got:\n${local.log}`);
}
if (local.notices.length) throw new Error(`local memory: unexpected notices: ${local.notices}`);
const bound = await startup("acme/kb");
if (bound.log !== "funes-index.sh --publish acme/kb\nsession.funes.jsonl\n") {
  throw new Error(`memory bound: expected the publish worker to find the seeded session, got:\n${bound.log}`);
}
// A funes that answers the handshake and exits: the next request meets a closed pipe. The
// extension still loads, indexes what it can, and says recall is unavailable.
const dying = await startup("", "exec head -n 1");
if (dying.log !== "funes-index.sh\nsession.funes.jsonl\n") {
  throw new Error(`dying funes: expected the index worker all the same, got:\n${dying.log}`);
}
if (!dying.notices.some((text) => text.startsWith("funes recall is unavailable"))) {
  throw new Error(`dying funes: expected the start to say recall is unavailable, got: ${dying.notices}`);
}

// Per turn, under pi's own root: the session that spoke and the one another project wrote since
// the last sweep, and never a file beside the root.
{
  const root = mkdtempSync(join(tmpdir(), "funes-pi-turn-"));
  const sessions = join(root, "sessions");
  const got = await turn(root, join(sessions, "--Users-me-repo--/a.jsonl"), join(sessions, "--Users-me-other--/b.jsonl"), join(root, "beside/x.jsonl"));
  if (!got.named || !got.stale) throw new Error(`under the root: expected both sessions converted, got ${JSON.stringify(got)}`);
  if (got.foreign) throw new Error("under the root: a file beside the root was converted");
  if (!got.swept) throw new Error("under the root: the sweep left no mark");
}
// A session pi was pointed elsewhere (`--session-dir`) sits in that directory with no directory
// per project: its own directory is swept, and nothing above it.
{
  const root = mkdtempSync(join(tmpdir(), "funes-pi-turn-"));
  const custom = join(root, "custom");
  const got = await turn(root, join(custom, "a.jsonl"), join(custom, "b.jsonl"), join(root, "other/x.jsonl"));
  if (!got.named || !got.stale) throw new Error(`elsewhere: expected both sessions converted, got ${JSON.stringify(got)}`);
  if (got.foreign) throw new Error("elsewhere: a file above the session's directory was converted");
}
process.exit(0);
