// Talking to the Go server, and turning its answers into what the page shows.

export type Language = "c" | "wat";
export type Mode = "jit" | "interp";

export interface Example {
  name: string;
  language: Language;
  source: string;
}

export interface CompileResult {
  id?: string;
  size?: number;
  compile_ms?: number;
  error?: string;
  warnings?: string;
}

export interface Stats {
  mode: Mode;
  decode_ms: number;
  validate_ms: number;
  compile_ms: number;
  instantiate_ms: number;
  run_ms: number;
  code_bytes: number;
  exit: number;
}

export interface RunResult {
  stdout: string;
  stderr: string;
  exit: number;
  trap?: string;
  killed: boolean;
  wall_ms: number;
  stats?: Stats;
  error?: string;
}

export interface RunOptions {
  args: string[];
  mode: Mode;
  timeout_ms: number;
  memory_mb: number;
}

async function postJSON<T>(url: string, body: unknown): Promise<T> {
  const r = await fetch(url, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  return (await r.json()) as T;
}

export const api = {
  examples: async (): Promise<Example[]> => (await fetch("api/examples")).json(),
  compile: (language: Language, source: string) => postJSON<CompileResult>("api/compile", { language, source }),
  run: (id: string, opts: RunOptions) => postJSON<RunResult>(`api/functions/${id}/run`, opts),
};

/** Splits an argument line like a shell would for simple cases: spaces separate, quotes group. */
export function splitArgs(line: string): string[] {
  const out: string[] = [];
  const re = /"([^"]*)"|'([^']*)'|(\S+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(line))) out.push(m[1] ?? m[2] ?? m[3]);
  return out;
}

export type Outcome = { kind: "ok" | "exit" | "trap" | "killed" | "error"; text: string };

/** One line summarising how a run ended. */
export function describeOutcome(r: RunResult): Outcome {
  if (r.error) return { kind: "error", text: r.error };
  if (r.killed) return { kind: "killed", text: "Killed: the process outlived its deadline." };
  if (r.trap) {
    const why: Record<string, string> = {
      interrupted: "the deadline passed",
      "out of bounds memory access": "the module touched memory outside its own",
      "call stack exhausted": "the call stack limit was reached",
    };
    return { kind: "trap", text: `Trap: ${r.trap}${why[r.trap] ? ` (${why[r.trap]})` : ""}.` };
  }
  if (r.exit === 0) return { kind: "ok", text: "Finished, exit code 0." };
  return { kind: "exit", text: `Finished with exit code ${r.exit}.` };
}

/** Timing rows for the page: [label, milliseconds]. */
export function timingRows(compileMs: number | undefined, r: RunResult): [string, number][] {
  const rows: [string, number][] = [];
  if (compileMs !== undefined) rows.push(["Source to Wasm (clang / wat2wasm)", compileMs]);
  const s = r.stats;
  if (s) {
    rows.push(["Decode", s.decode_ms], ["Validate", s.validate_ms]);
    if (s.mode === "jit") rows.push(["JIT compile", s.compile_ms]);
    rows.push(["Instantiate (incl. JIT)", s.instantiate_ms], ["Run", s.run_ms]);
  }
  if (typeof r.wall_ms === "number") rows.push(["Whole request (process start to exit)", r.wall_ms]);
  return rows;
}

export function formatMs(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)} µs`;
  if (ms < 100) return `${ms.toFixed(1)} ms`;
  return `${Math.round(ms).toLocaleString("en")} ms`;
}
