// Whether two files were last modified within a second of each other. A JS runtime sets a file's
// time to the millisecond, so a stamp copied from a session equals the session's only to that
// precision — and a shell's `-nt` compares nanoseconds where the filesystem keeps them.
//
// Usage: same-second.mjs <a> <b>; exits 0 when they match.
import { statSync } from "node:fs";

const [a, b] = process.argv.slice(2).map((path) => statSync(path).mtimeMs);
process.exit(Math.abs(a - b) < 1000 ? 0 : 1);
