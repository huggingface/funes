// funes-owned pi extension: expose recall over past AI-assistant sessions as
// first-class pi tools, and index each turn into the local memory.
//
// pi has no MCP client, so this extension *is* the client: it spawns `funes mcp`
// once over stdio, keeps it warm for the session, and forwards each call as an
// MCP `tools/call`. That keeps the embedder + reranker loaded across calls
// (unlike shelling out to `funes recall`, which reloads both every time), and
// it consumes the same `funes mcp` surface every other agent integration uses.
//
// The tools aren't written out here: the extension registers whatever
// `tools/list` returns, passing each MCP schema through as pi's `parameters` —
// so there is no second copy of the surface to keep in step.
//
// Install:  funes add pi   — or, from a funes checkout, `pi install
// ./integrations/pi`, or `pi -e ./integrations/pi` for a single run.
//
// `funes` is taken from PATH; set FUNES_BIN to override the binary. FUNES_BIN and
// FUNES_MEMORY are generic environment overrides funes honors; a host that embeds
// funes sets them from the outside. (agentcap's funes example is one such host —
// that example depends on funes, not the reverse: it puts its own `funes` on PATH
// and points FUNES_MEMORY at a live hf:// memory. This extension knows nothing of
// it — only of the vars.)
//
// The memory this extension recalls from — and publishes to — is resolved once, in order:
//   1. FUNES_MEMORY in the environment (a per-run override, set by whatever host)
//   2. the memory bound at install by `funes add pi <memory>`, saved in a `memory`
//      file next to this extension (absent = the local memory)
// The result is forwarded as the `funes mcp <memory>` positional; empty forwards a
// bare `funes mcp` (the local memory).
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const FUNES_BIN = process.env.FUNES_BIN || "funes";

// The memory `funes add pi <memory>` wrote beside this extension, or "" if none (local).
function boundMemory(): string {
  try {
    return readFileSync(join(HERE, "memory"), "utf8").trim();
  } catch {
    return "";
  }
}

const memory = (process.env.FUNES_MEMORY || boundMemory()).trim();
const FUNES_ARGS = memory ? ["mcp", memory] : ["mcp"];
const PROTOCOL_VERSION = "2024-11-05"; // matches funes' rmcp server
const CALL_TIMEOUT_MS = 120_000;
const HANDSHAKE_TIMEOUT_MS = 10_000; // pi's startup waits on these, so they don't get a recall's bound

type Pending = { resolve: (v: any) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> };
type McpTool = { name: string; description?: string; inputSchema: Record<string, unknown> };

// A minimal MCP stdio client for a single `funes mcp` child. stdout is the
// JSON-RPC channel (newline-delimited messages); stderr is logs.
class FunesMcp {
  private child?: ChildProcessWithoutNullStreams;
  private ready?: Promise<void>;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private buf = "";

  private ensureStarted(): Promise<void> {
    if (this.child && this.ready) return this.ready;
    const child = spawn(FUNES_BIN, FUNES_ARGS, { stdio: ["pipe", "pipe", "pipe"] });
    this.child = child;
    // The server is kept warm for the whole session, so unref it (and its pipes)
    // — an in-flight call's timer keeps the loop alive, but once the agent's turn
    // is done nothing should. Without this the idle child pins the host's event
    // loop open and the turn never exits.
    child.unref();
    child.stdin.unref();
    child.stdout.unref();
    child.stderr.unref();
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => this.onData(chunk));
    child.stderr.resume(); // drain logs so the pipe never blocks
    const die = (err: Error) => {
      this.child = undefined;
      this.ready = undefined;
      for (const p of this.pending.values()) {
        clearTimeout(p.timer);
        p.reject(err);
      }
      this.pending.clear();
    };
    child.on("exit", (code) => die(new Error(`funes mcp exited (code ${code})`)));
    child.on("error", (e) => die(new Error(`funes mcp failed to start: ${e.message}`)));

    const start = (async () => {
      try {
        await this.request(
          "initialize",
          {
            protocolVersion: PROTOCOL_VERSION,
            capabilities: {},
            clientInfo: { name: "pi-funes-bridge", version: "0.1.0" },
          },
          undefined,
          HANDSHAKE_TIMEOUT_MS,
        );
        this.send({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });
      } catch (err: any) {
        // Initialize failed while the child may still be alive (e.g. it timed out): tear it
        // down so the next call respawns instead of re-awaiting this rejected promise forever.
        if (this.ready === start) {
          this.child?.kill();
          this.child = undefined;
          this.ready = undefined;
        }
        throw err;
      }
    })();
    this.ready = start;
    return start;
  }

  private onData(chunk: string) {
    this.buf += chunk;
    let nl: number;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl).trim();
      this.buf = this.buf.slice(nl + 1);
      if (!line) continue;
      let msg: any;
      try {
        msg = JSON.parse(line);
      } catch {
        continue; // not a JSON-RPC frame (stray output)
      }
      const p = typeof msg.id === "number" ? this.pending.get(msg.id) : undefined;
      if (!p) continue;
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.error) p.reject(new Error(msg.error.message || JSON.stringify(msg.error)));
      else p.resolve(msg.result);
    }
  }

  private send(obj: any) {
    if (!this.child) throw new Error("funes mcp not running");
    this.child.stdin.write(JSON.stringify(obj) + "\n");
  }

  private request(method: string, params: any, signal?: AbortSignal, timeoutMs = CALL_TIMEOUT_MS): Promise<any> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const settle = (err: Error) => {
        const p = this.pending.get(id);
        if (!p) return;
        this.pending.delete(id);
        clearTimeout(p.timer);
        reject(err);
      };
      const timer = setTimeout(() => settle(new Error(`funes ${method} timed out`)), timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      // A cancelled turn releases its call here instead of holding the event loop until the timeout.
      signal?.addEventListener("abort", () => settle(new Error(`funes ${method} cancelled`)), { once: true });
      try {
        this.send({ jsonrpc: "2.0", id, method, params });
      } catch (err: any) {
        settle(err);
      }
    });
  }

  // The tools this funes binary exposes, as MCP declares them.
  async listTools(): Promise<McpTool[]> {
    await this.ensureStarted();
    const result = await this.request("tools/list", {}, undefined, HANDSHAKE_TIMEOUT_MS);
    return (result?.tools ?? []) as McpTool[];
  }

  // Call an MCP tool and flatten its text content to a string.
  async callTool(name: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<string> {
    await this.ensureStarted();
    const result = await this.request("tools/call", { name, arguments: args }, signal);
    const content: any[] = result?.content ?? [];
    return content
      .filter((c) => c?.type === "text")
      .map((c) => c.text)
      .join("\n");
  }
}

const funes = new FunesMcp();

// The automation scripts `funes add pi` installs beside this extension. A checkout has none, hence
// the guard in runScript: a missing script means no automation, not an error.
const INDEX_SH = join(HERE, "scripts", "funes-index.sh");
const PUSH_SH = join(HERE, "scripts", "funes-push.sh");
// Both scripts take the harness to index: the push runs that index itself, so a boundary publishes
// the turns a detached per-turn worker may not have stored yet.
const HARNESS = "pi";

// Run one automation script and forget it: each hands off to a detached worker, and no failure of
// theirs is worth disturbing the session with.
function runScript(script: string, ...args: string[]) {
  if (!existsSync(script)) return;
  try {
    const child = spawn("bash", [script, ...args], { detached: true, stdio: "ignore" });
    child.on("error", () => {});
    child.unref();
  } catch {}
}

export default async function (pi: any) {
  let tools: McpTool[] = [];
  let failure = "";
  try {
    tools = await funes.listTools();
  } catch (e: any) {
    failure = `funes recall is unavailable: ${e?.message || String(e)}`;
  }

  for (const tool of tools) {
    pi.registerTool({
      name: tool.name,
      label: `funes ${tool.name}`,
      description: tool.description ?? "",
      parameters: tool.inputSchema,
      execute: async (_id: string, params: Record<string, unknown>, signal?: AbortSignal) => {
        try {
          return { content: [{ type: "text", text: await funes.callTool(tool.name, params, signal) }], details: {} };
        } catch (e: any) {
          return { content: [{ type: "text", text: `${tool.name} error: ${e?.message || String(e)}` }], details: {} };
        }
      },
    });
  }

  pi.on("session_start", async (event: any, ctx: any) => {
    if (failure) ctx?.ui?.notify(failure, "warning"); // at load it would land inside pi's own startup
    // A fresh process is the one start with no shutdown behind it, so it catches up whatever a
    // process that never shut down cleanly left unpublished.
    if (memory && event?.reason === "startup") runScript(PUSH_SH, memory, HARNESS);
  });

  // Per turn, so a session killed mid-flight is indexed up to its last completed turn.
  pi.on("turn_end", async () => runScript(INDEX_SH, HARNESS));

  // A reload replaces the extension instance without ending the session: nothing to publish.
  pi.on("session_shutdown", async (event: any) => {
    if (memory && event?.reason !== "reload") runScript(PUSH_SH, memory, HARNESS);
  });
}
