// The extension's startup with a history `setup` could not convert (no JS runtime on PATH): it is
// converted before the worker that indexes it is launched — the publish worker with a memory bound,
// which indexes first, and the index worker without. Loads index.ts as pi would, with a fake pi, and
// reads what the worker saw in the spool when it started.
//
//   node integrations/pi/test/startup.mjs    (run.sh runs it)
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
// `cat` echoes each request back as its reply: a server with no tools, enough to load.
process.env.FUNES_BIN = "cat";
delete process.env.FUNES_MEMORY;

async function startup(memory) {
  const root = mkdtempSync(join(tmpdir(), "funes-pi-startup-"));
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
  // Each worker records what it was launched for and what the spool held when it started.
  const log = join(root, "worker.log");
  for (const name of ["funes-index.sh", "funes-push.sh"]) {
    const script = join(ext, "scripts", name);
    writeFileSync(script, `#!/bin/sh\n{ printf '%s %s\\n' "${name}" "$*"; ls "${spool}"; } >"${log}.tmp" && mv "${log}.tmp" "${log}"\n`);
    chmodSync(script, 0o755);
  }

  const handlers = {};
  const pi = { registerTool() {}, on(name, fn) { handlers[name] = fn; } };
  const { default: activate } = await import(pathToFileURL(join(ext, "index.ts")).href);
  await activate(pi);
  await handlers.session_start({ reason: "startup" }, {});

  if (existsSync(join(ext, "seed-pending"))) throw new Error("the pending history was not converted");
  const deadline = Date.now() + 10_000;
  while (!existsSync(log)) {
    if (Date.now() > deadline) throw new Error(`no worker ran for ${memory || "the local memory"}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return readFileSync(log, "utf8");
}

const local = await startup("");
if (local !== "funes-index.sh pi\nsession.funes.jsonl\n") {
  throw new Error(`local memory: expected the index worker to find the seeded session, got:\n${local}`);
}
const bound = await startup("acme/kb");
if (bound !== "funes-push.sh acme/kb pi\nsession.funes.jsonl\n") {
  throw new Error(`memory bound: expected the publish worker to find the seeded session, got:\n${bound}`);
}
process.exit(0);
