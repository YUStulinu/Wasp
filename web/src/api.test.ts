import { describe, expect, it } from "vitest";
import { type RunResult, describeOutcome, formatMs, splitArgs, timingRows } from "./api";

const base: RunResult = { stdout: "", stderr: "", exit: 0, killed: false, wall_ms: 12 };

describe("api helpers", () => {
  it("splits arguments with quotes", () => {
    expect(splitArgs(`a "b c" 'd e' f`)).toEqual(["a", "b c", "d e", "f"]);
    expect(splitArgs("   ")).toEqual([]);
  });

  it("describes how a run ended", () => {
    expect(describeOutcome(base).kind).toBe("ok");
    expect(describeOutcome({ ...base, exit: 3 }).text).toContain("exit code 3");
    expect(describeOutcome({ ...base, trap: "interrupted" }).text).toContain("deadline");
    expect(describeOutcome({ ...base, killed: true }).kind).toBe("killed");
    expect(describeOutcome({ ...base, error: "unknown function" }).kind).toBe("error");
  });

  it("lists timings, with the JIT step only in JIT mode", () => {
    const stats = { mode: "jit" as const, decode_ms: 1, validate_ms: 2, compile_ms: 3, instantiate_ms: 4, run_ms: 5, code_bytes: 10, exit: 0 };
    const rows = timingRows(7, { ...base, stats });
    expect(rows.map((r) => r[0])).toContain("JIT compile");
    expect(rows[0]).toEqual(["Source to Wasm (clang / wat2wasm)", 7]);
    const interp = timingRows(undefined, { ...base, stats: { ...stats, mode: "interp" } });
    expect(interp.map((r) => r[0])).not.toContain("JIT compile");
  });

  it("formats durations", () => {
    expect(formatMs(0.25)).toBe("250 µs");
    expect(formatMs(12.34)).toBe("12.3 ms");
    expect(formatMs(1234.5)).toBe("1,235 ms");
  });
});
