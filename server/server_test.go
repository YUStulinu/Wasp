package main

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
)

func waspPath(t *testing.T) string {
	exe := ""
	if runtime.GOOS == "windows" {
		exe = ".exe"
	}
	p := filepath.Join("..", "runtime", "target", "release", "wasp"+exe)
	if env := os.Getenv("WASP"); env != "" {
		p = env
	}
	if _, err := os.Stat(p); err != nil {
		t.Skip("build the runtime first (cargo build --release in runtime/)")
	}
	abs, _ := filepath.Abs(p)
	return abs
}

func newTestServer(t *testing.T) (*Server, *httptest.Server) {
	s := NewServer(waspPath(t), "clang", filepath.Join("..", "examples"), t.TempDir(), filepath.Join("..", "web", "examples"), 4)
	ts := httptest.NewServer(s.Routes(""))
	t.Cleanup(ts.Close)
	return s, ts
}

func post(t *testing.T, url string, body any) map[string]any {
	b, _ := json.Marshal(body)
	resp, err := http.Post(url, "application/json", bytes.NewReader(b))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var out map[string]any
	json.NewDecoder(resp.Body).Decode(&out)
	return out
}

const helloWat = `(module
  (import "wasi_snapshot_preview1" "fd_write" (func $w (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "hello, server\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 16))
    (i32.store (i32.const 4) (i32.const 14))
    (drop (call $w (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))`

func compileWat(t *testing.T, ts *httptest.Server, src string) string {
	r := post(t, ts.URL+"/api/compile", map[string]string{"language": "wat", "source": src})
	id, ok := r["id"].(string)
	if !ok {
		t.Fatalf("compile failed: %v", r)
	}
	return id
}

func TestCompileAndRun(t *testing.T) {
	_, ts := newTestServer(t)
	id := compileWat(t, ts, helloWat)
	for _, mode := range []string{"jit", "interp"} {
		r := post(t, ts.URL+"/api/functions/"+id+"/run", map[string]any{"mode": mode})
		if r["stdout"] != "hello, server\n" || r["exit"].(float64) != 0 {
			t.Fatalf("%s: unexpected result %v", mode, r)
		}
		if r["stats"] == nil {
			t.Fatalf("%s: no stats", mode)
		}
	}
}

func TestTimeoutStopsInfiniteLoop(t *testing.T) {
	_, ts := newTestServer(t)
	id := compileWat(t, ts, `(module (func (export "_start") (loop $l (br $l))))`)
	r := post(t, ts.URL+"/api/functions/"+id+"/run", map[string]any{"timeout_ms": 300})
	if r["trap"] != "interrupted" {
		t.Fatalf("expected an interrupt, got %v", r)
	}
	if r["wall_ms"].(float64) > 2000 {
		t.Fatalf("took %v ms", r["wall_ms"])
	}
}

func TestRejectsGarbageAndBadIDs(t *testing.T) {
	_, ts := newTestServer(t)
	resp, _ := http.Post(ts.URL+"/api/functions", "application/wasm", strings.NewReader("not wasm"))
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("garbage accepted: %d", resp.StatusCode)
	}
	r := post(t, ts.URL+"/api/functions/not-an-id/run", map[string]any{})
	if r["error"] == nil {
		t.Fatalf("bad id accepted: %v", r)
	}
	r = post(t, ts.URL+"/api/compile", map[string]string{"language": "wat", "source": "(module (func (i32.add)))"})
	if r["error"] == nil {
		t.Fatalf("invalid module compiled: %v", r)
	}
}

func TestEscapeAttemptIsContained(t *testing.T) {
	_, ts := newTestServer(t)
	src, err := os.ReadFile(filepath.Join("..", "security", "wasi_escape.wat"))
	if err != nil {
		t.Skip(err)
	}
	id := compileWat(t, ts, string(src))
	r := post(t, ts.URL+"/api/functions/"+id+"/run", map[string]any{})
	if r["exit"].(float64) != 0 {
		t.Fatalf("escape attempt not refused: %v", r)
	}
}

func TestParallelRuns(t *testing.T) {
	s, ts := newTestServer(t)
	id := compileWat(t, ts, helloWat)
	var wg sync.WaitGroup
	errs := make(chan string, 16)
	for i := 0; i < 16; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			res, err := s.Run(context.Background(), id, runRequest{})
			if err != nil || res.Stdout != "hello, server\n" {
				errs <- "bad run"
			}
		}()
	}
	wg.Wait()
	close(errs)
	for e := range errs {
		t.Fatal(e)
	}
}
