// The extension's startup with a history `setup` could not convert (no JS runtime on PATH): it is
// converted before the worker that indexes it is launched — the publish worker with a memory bound,
// which indexes first, and the index worker without. And a `funes` that dies as the session starts
// costs recall and nothing else. Loads index.ts as pi would, with a fake pi and a fake `funes mcp`,
// and reads what the worker saw in the spool when it started.
//
//   node integrations/pi/test/startup.mjs    (run.sh runs it)
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
delete process.env.FUNES_MEMORY;

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
  const ext = join(root, "ext");
  mkdirSync(join(ext, "scripts"), { recursive: true });
  for (const name of ["index.ts", "convert.mjs"]) cpSync(join(HERE, "..", name), join(ext, name));
  const sessions = join(root, "sessions");
  mkdirSync(join(sessions, "2026/09/25"), { recursive: true });
  cpSync(join(HERE, "session.jsonl"), join(sessions, "2026/09/25/session.jsonl"));
  const spool = join(root, "spool");
  mkdirSync(spool);
  writeFileSync(join(ext, "spool"), `${spool}\n`);
  writeFileSync(join(ext, "seed-pending"), `${sessions}\n`);
  if (memory) writeFileSync(join(ext, "memory"), `${memory}\n`);
  // The worker records what it was launched for and what the spool held when it started.
  const log = join(root, "worker.log");
  const script = join(ext, "scripts", "funes-index.sh");
  writeFileSync(script, `#!/bin/sh\n{ printf 'funes-index.sh%s\\n' "${"${*:+ $*}"}"; ls "${spool}"; } >"${log}.tmp" && mv "${log}.tmp" "${log}"\n`);
  chmodSync(script, 0o755);

  const handlers = {};
  const notices = [];
  const pi = { registerTool() {}, on(name, fn) { handlers[name] = fn; } };
  const { default: activate } = await import(pathToFileURL(join(ext, "index.ts")).href);
  await activate(pi);
  await handlers.session_start({ reason: "startup" }, { ui: { notify: (text) => notices.push(text) } });

  if (existsSync(join(ext, "seed-pending"))) throw new Error("the pending history was not converted");
  const deadline = Date.now() + 10_000;
  while (!existsSync(log)) {
    if (Date.now() > deadline) throw new Error(`no worker ran for ${memory || "the local memory"}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return { log: readFileSync(log, "utf8"), notices };
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
process.exit(0);
