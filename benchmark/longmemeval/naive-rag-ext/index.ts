import { Type } from "typebox";
import { execFileSync } from "node:child_process";

const PY = process.env.NAIVE_PY || "python3";
const SCRIPT = process.env.NAIVE_SEARCH || "naive_search.py";

export default function (pi: any) {
  pi.registerTool({
    name: "dense_search",
    label: "Dense search",
    description:
      "Search the user's past conversations by semantic similarity (dense retrieval). " +
      "Returns the most relevant past messages with their dates.",
    parameters: Type.Object({
      query: Type.String({ description: "What to look for in the user's past conversations." }),
      k: Type.Optional(Type.Number({ description: "Number of results to return (default 8)." })),
    }),
    execute: (_id: string, p: { query: string; k?: number }) => {
      const out = execFileSync(PY, [SCRIPT, p.query, String(p.k ?? 8)], {
        encoding: "utf8",
        env: process.env,
        maxBuffer: 16 * 1024 * 1024,
      });
      return { content: [{ type: "text", text: out.trim() || "(no results)" }] };
    },
  });
}
