// funes-owned pi extension: expose recall over past AI-assistant sessions as
// first-class pi tools (`recall`, `get`).
//
// pi has no MCP client, so this extension *is* the client: it spawns `funes mcp`
// once over stdio, keeps it warm for the session, and forwards each call as an
// MCP `tools/call`. That keeps the embedder + reranker loaded across calls
// (unlike shelling out to `funes recall`, which reloads both every time), and
// it consumes the same `funes mcp` surface every other agent integration uses.
//
// Install:  funes install pi   — or, from a funes checkout, `pi install
// ./integrations/pi`, or drop this dir under <cwd>/.pi/extensions/ for
// zero-config discovery.
//
// `funes` is taken from PATH; set FUNES_BIN to override — e.g. inside an
// agentcap sandbox, --tool-dir puts the bundle's `funes` wrapper on PATH (which
// also points FASTEMBED_CACHE_DIR at the prewarmed cache and FUNES_HOME at the
// live-remote config).
import { Type } from "typebox";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

const FUNES_BIN = process.env.FUNES_BIN || "funes";
const PROTOCOL_VERSION = "2024-11-05"; // matches funes' rmcp server
const CALL_TIMEOUT_MS = 120_000;

type Pending = { resolve: (v: any) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> };

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
    const child = spawn(FUNES_BIN, ["mcp"], { stdio: ["pipe", "pipe", "pipe"] });
    this.child = child;
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

    this.ready = (async () => {
      await this.request("initialize", {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: {},
        clientInfo: { name: "pi-funes-bridge", version: "0.1.0" },
      });
      this.send({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });
    })();
    return this.ready;
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

  private request(method: string, params: any): Promise<any> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`funes ${method} timed out`));
      }, CALL_TIMEOUT_MS);
      this.pending.set(id, { resolve, reject, timer });
      this.send({ jsonrpc: "2.0", id, method, params });
    });
  }

  // Call an MCP tool and flatten its text content to a string.
  async callTool(name: string, args: Record<string, unknown>): Promise<string> {
    await this.ensureStarted();
    const result = await this.request("tools/call", { name, arguments: args });
    const content: any[] = result?.content ?? [];
    return content
      .filter((c) => c?.type === "text")
      .map((c) => c.text)
      .join("\n");
  }
}

const funes = new FunesMcp();

async function call(name: string, args: Record<string, unknown>) {
  try {
    return { content: [{ type: "text", text: await funes.callTool(name, args) }] };
  } catch (e: any) {
    return { content: [{ type: "text", text: `${name} error: ${e?.message || String(e)}` }] };
  }
}

export default function (pi: any) {
  pi.registerTool({
    name: "recall",
    label: "Recall",
    description:
      "Recall decisions, rationale, and context from the user's past AI-assistant sessions. " +
      "Returns ranked passages with provenance (timestamp, session, block type); each hit " +
      "carries a `→ get <session_id> <turn_uuid>` line you can pass to `get` to read the full " +
      "surrounding turns. Use it when a question concerns something decided or discussed in an " +
      "earlier session rather than the current files, or before asserting the history of anything.",
    parameters: Type.Object({
      query: Type.String({ description: "Natural-language search query" }),
      k: Type.Optional(Type.Number({ description: "Number of results (default 8)" })),
    }),
    execute: (_id: string, params: { query: string; k?: number }) =>
      call("recall", params.k ? { query: params.query, k: params.k } : { query: params.query }),
  });

  pi.registerTool({
    name: "get",
    label: "Recall: get",
    description:
      "Drill down on a recall hit: fetch the named turn plus the turns around it, reassembled " +
      "into readable text. Pass the `session_id` and `turn_uuid` from a recall hit's `→ get` line.",
    parameters: Type.Object({
      session_id: Type.String({ description: "Session id from a recall hit's `→ get` line" }),
      turn_uuid: Type.String({ description: "Turn uuid from a recall hit's `→ get` line" }),
      window: Type.Optional(Type.Number({ description: "Turns within this window are included (default 3)" })),
    }),
    execute: (_id: string, params: { session_id: string; turn_uuid: string; window?: number }) =>
      call("get", params.window ? params : { session_id: params.session_id, turn_uuid: params.turn_uuid }),
  });
}
