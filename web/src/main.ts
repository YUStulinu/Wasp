import "./style.css";
import { type Example, type Language, type Mode, api, describeOutcome, formatMs, splitArgs, timingRows } from "./api";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const source = $<HTMLTextAreaElement>("source");
const gutter = $<HTMLPreElement>("gutter");
let examples: Example[] = [];

function updateGutter() {
  const n = source.value.split("\n").length;
  gutter.textContent = Array.from({ length: n }, (_, i) => String(i + 1)).join("\n");
  gutter.scrollTop = source.scrollTop;
}

function loadExample(i: number) {
  const ex = examples[i];
  if (!ex) return;
  source.value = ex.source;
  $<HTMLSelectElement>("language").value = ex.language;
  updateGutter();
}

function setStatus(kind: string, text: string) {
  const s = $("status");
  s.className = `status ${kind}`;
  s.textContent = text;
}

async function run() {
  const button = $<HTMLButtonElement>("run");
  button.disabled = true;
  setStatus("busy", "Compiling…");
  $("stdout").textContent = "";
  $("stderr").textContent = "";
  $("stderr").hidden = $("stderr-title").hidden = true;
  $("timings").innerHTML = "";
  $("sizes").textContent = "";
  try {
    const t0 = performance.now();
    const c = await api.compile($<HTMLSelectElement>("language").value as Language, source.value);
    if (c.error || !c.id) {
      setStatus("error", "Compilation failed.");
      $("stderr").textContent = c.error ?? "unknown error";
      $("stderr").hidden = $("stderr-title").hidden = false;
      return;
    }
    setStatus("busy", "Running…");
    const r = await api.run(c.id, {
      args: splitArgs($<HTMLInputElement>("args").value),
      mode: $<HTMLSelectElement>("mode").value as Mode,
      timeout_ms: Number($<HTMLInputElement>("timeout").value) || 2000,
      memory_mb: Number($<HTMLInputElement>("memory").value) || 64,
    });
    const total = performance.now() - t0;
    const o = describeOutcome(r);
    setStatus(o.kind, o.text);
    $("stdout").textContent = r.stdout || "(no output)";
    if (r.stderr) {
      $("stderr").textContent = r.stderr;
      $("stderr").hidden = $("stderr-title").hidden = false;
    }
    const rows = timingRows(c.compile_ms, r);
    rows.push(["Round trip from this page", total]);
    $("timings").innerHTML = rows.map(([k, v]) => `<tr><td>${k}</td><td>${formatMs(v)}</td></tr>`).join("");
    $("sizes").textContent =
      `Wasm module: ${c.size?.toLocaleString("en")} bytes` + (r.stats?.code_bytes ? ` · machine code: ${r.stats.code_bytes.toLocaleString("en")} bytes` : "");
  } catch (e) {
    setStatus("error", `Could not reach the server: ${e}`);
  } finally {
    button.disabled = false;
  }
}

source.addEventListener("input", updateGutter);
source.addEventListener("scroll", () => (gutter.scrollTop = source.scrollTop));
source.addEventListener("keydown", (e) => {
  if (e.key === "Tab") {
    e.preventDefault();
    const { selectionStart: a, selectionEnd: b } = source;
    source.setRangeText("    ", a, b, "end");
    updateGutter();
  } else if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    void run();
  }
});
$("run").addEventListener("click", () => void run());
$<HTMLSelectElement>("example").addEventListener("change", (e) => loadExample(Number((e.target as HTMLSelectElement).value)));

try {
  examples = await api.examples();
  $<HTMLSelectElement>("example").innerHTML = examples
    .map((ex, i) => `<option value="${i}">${ex.name.replace(/^\d+-/, "").replace(/\.(c|wat)$/, "").replace(/-/g, " ")} (${ex.language === "c" ? "C" : "WAT"})</option>`)
    .join("");
  loadExample(0);
} catch {
  setStatus("error", "The server is not running: start it with `go run .` in server/.");
}
