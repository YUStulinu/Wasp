// Command server runs WebAssembly "functions" in isolation, one `wasp` process per call, and
// serves the playground.
//
//	server -addr :8090 -wasp ../runtime/target/release/wasp -static ../web/dist
//
// API:
//
//	POST /api/compile          {"language": "c"|"wat", "source": "..."} -> {"id", "size", "compile_ms"} or {"error"}
//	POST /api/functions        raw .wasm body -> {"id", "size"}
//	POST /api/functions/{id}/run {"args": [], "mode": "jit"|"interp", "timeout_ms", "memory_mb"}
//	                           -> {"stdout", "stderr", "exit", "trap", "killed", "wall_ms", "stats"}
//	GET  /api/examples         example programs for the playground
//
// Isolation: every call runs in a separate OS process with the runtime's own limits (linear
// memory cap, deadline enforced by interrupting compiled code, call-depth and frame limits),
// an empty scratch directory as its only visible file system, and a hard kill if the process
// outlives its deadline.
package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strings"
	"sync"
	"time"
)

type Server struct {
	Wasp     string // path to the wasp binary
	Clang    string // path to clang (for C sources)
	Include  string // directory with wasi_min.h
	Store    string // where modules are kept
	Examples string // directory with example programs
	sem      chan struct{}
	mu       sync.Mutex
}

const (
	maxModule  = 32 << 20
	maxOutput  = 1 << 20
	maxTimeout = 10_000
	maxMemory  = 256
)

func NewServer(wasp, clang, include, store, examples string, parallel int) *Server {
	os.MkdirAll(store, 0o755)
	// Runs happen in scratch directories: paths must not be relative.
	if abs, err := filepath.Abs(wasp); err == nil {
		wasp = abs
	}
	if abs, err := filepath.Abs(include); err == nil {
		include = abs
	}
	return &Server{Wasp: wasp, Clang: clang, Include: include, Store: store, Examples: examples, sem: make(chan struct{}, parallel)}
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

func errJSON(w http.ResponseWriter, code int, msg string) {
	writeJSON(w, code, map[string]string{"error": msg})
}

var idPattern = regexp.MustCompile(`^[0-9a-f]{16}$`)

// save stores a module under the hash of its bytes and returns the id.
func (s *Server) save(wasm []byte) (string, error) {
	if len(wasm) < 8 || !bytes.Equal(wasm[:4], []byte("\x00asm")) {
		return "", errors.New("not a WebAssembly module")
	}
	sum := sha256.Sum256(wasm)
	id := hex.EncodeToString(sum[:8])
	path := filepath.Join(s.Store, id+".wasm")
	if _, err := os.Stat(path); err != nil {
		if err := os.WriteFile(path, wasm, 0o644); err != nil {
			return "", err
		}
	}
	return id, nil
}

func (s *Server) handleUpload(w http.ResponseWriter, r *http.Request) {
	body, err := readLimited(r, maxModule)
	if err != nil {
		errJSON(w, http.StatusRequestEntityTooLarge, err.Error())
		return
	}
	id, err := s.save(body)
	if err != nil {
		errJSON(w, http.StatusBadRequest, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"id": id, "size": len(body)})
}

func readLimited(r *http.Request, n int64) ([]byte, error) {
	b, err := readAll(http.MaxBytesReader(nil, r.Body, n))
	if err != nil {
		return nil, fmt.Errorf("body too large or unreadable: %w", err)
	}
	return b, nil
}

func readAll(r interface{ Read([]byte) (int, error) }) ([]byte, error) {
	var buf bytes.Buffer
	_, err := buf.ReadFrom(r)
	return buf.Bytes(), err
}

type compileRequest struct {
	Language string `json:"language"`
	Source   string `json:"source"`
}

func (s *Server) handleCompile(w http.ResponseWriter, r *http.Request) {
	var req compileRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<20)).Decode(&req); err != nil {
		errJSON(w, http.StatusBadRequest, "bad request: "+err.Error())
		return
	}
	dir, err := os.MkdirTemp("", "wasp-compile-")
	if err != nil {
		errJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer os.RemoveAll(dir)
	out := filepath.Join(dir, "out.wasm")
	var cmd *exec.Cmd
	ctx, cancel := context.WithTimeout(r.Context(), 30*time.Second)
	defer cancel()
	switch req.Language {
	case "c":
		src := filepath.Join(dir, "main.c")
		os.WriteFile(src, []byte(req.Source), 0o644)
		cmd = exec.CommandContext(ctx, s.Clang, "--target=wasm32", "-O2", "-nostdlib", "-Wl,--no-entry",
			"-I", s.Include, "-o", out, src)
	case "wat":
		src := filepath.Join(dir, "main.wat")
		os.WriteFile(src, []byte(req.Source), 0o644)
		cmd = exec.CommandContext(ctx, s.Wasp, "wat2wasm", src, "-o", out)
	default:
		errJSON(w, http.StatusBadRequest, "language must be c or wat")
		return
	}
	t0 := time.Now()
	msg, err := cmd.CombinedOutput()
	ms := float64(time.Since(t0).Microseconds()) / 1000
	if err != nil {
		writeJSON(w, http.StatusOK, map[string]any{"error": strings.ReplaceAll(string(msg), dir+string(os.PathSeparator), ""), "compile_ms": ms})
		return
	}
	wasm, err := os.ReadFile(out)
	if err != nil {
		errJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	id, err := s.save(wasm)
	if err != nil {
		errJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"id": id, "size": len(wasm), "compile_ms": ms, "warnings": string(msg)})
}

type runRequest struct {
	Args      []string `json:"args"`
	Mode      string   `json:"mode"`
	TimeoutMs int      `json:"timeout_ms"`
	MemoryMB  int      `json:"memory_mb"`
}

// RunResult is what one call of a function returns.
type RunResult struct {
	Stdout string          `json:"stdout"`
	Stderr string          `json:"stderr"`
	Exit   int             `json:"exit"`
	Trap   string          `json:"trap,omitempty"`
	Killed bool            `json:"killed"`
	WallMs float64         `json:"wall_ms"`
	Stats  json.RawMessage `json:"stats,omitempty"`
}

type limitedBuffer struct {
	bytes.Buffer
	limit int
}

func (b *limitedBuffer) Write(p []byte) (int, error) {
	if room := b.limit - b.Len(); room > 0 {
		if len(p) > room {
			b.Buffer.Write(p[:room])
		} else {
			b.Buffer.Write(p)
		}
	}
	return len(p), nil
}

// Run executes a stored module with the given limits.
func (s *Server) Run(ctx context.Context, id string, req runRequest) (*RunResult, error) {
	if !idPattern.MatchString(id) {
		return nil, errors.New("bad id")
	}
	path := filepath.Join(s.Store, id+".wasm")
	if _, err := os.Stat(path); err != nil {
		return nil, errors.New("unknown function")
	}
	if req.TimeoutMs <= 0 || req.TimeoutMs > maxTimeout {
		req.TimeoutMs = 2000
	}
	if req.MemoryMB <= 0 || req.MemoryMB > maxMemory {
		req.MemoryMB = 64
	}
	mode := "--jit"
	if req.Mode == "interp" {
		mode = "--interp"
	}
	scratch, err := os.MkdirTemp("", "wasp-run-")
	if err != nil {
		return nil, err
	}
	defer os.RemoveAll(scratch)
	s.sem <- struct{}{}
	defer func() { <-s.sem }()
	// The runtime interrupts the guest at the deadline; the process is killed if it is still
	// alive a second later.
	ctx, cancel := context.WithTimeout(ctx, time.Duration(req.TimeoutMs+1000)*time.Millisecond)
	defer cancel()
	args := []string{"run", mode, "--stats", "--timeout-ms", fmt.Sprint(req.TimeoutMs),
		"--max-memory-mb", fmt.Sprint(req.MemoryMB), "--dir", scratch + "::/", "--argv0", id + ".wasm", path}
	args = append(args, req.Args...)
	cmd := exec.CommandContext(ctx, s.Wasp, args...)
	cmd.Dir = scratch
	cmd.Env = []string{}
	var stdout, stderr limitedBuffer
	stdout.limit, stderr.limit = maxOutput, maxOutput
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	t0 := time.Now()
	err = cmd.Run()
	res := &RunResult{WallMs: float64(time.Since(t0).Microseconds()) / 1000, Stdout: stdout.String()}
	var lines []string
	for _, line := range strings.Split(stderr.String(), "\n") {
		switch {
		case strings.HasPrefix(line, "{\"mode\""):
			res.Stats = json.RawMessage(line)
		case strings.HasPrefix(line, "trap: "):
			res.Trap = strings.TrimPrefix(line, "trap: ")
		case line != "":
			lines = append(lines, line)
		}
	}
	res.Stderr = strings.Join(lines, "\n")
	if ctx.Err() == context.DeadlineExceeded {
		res.Killed = true
	}
	var exitErr *exec.ExitError
	if errors.As(err, &exitErr) {
		res.Exit = exitErr.ExitCode()
	} else if err != nil && !res.Killed {
		return nil, err
	}
	return res, nil
}

func (s *Server) handleRun(w http.ResponseWriter, r *http.Request) {
	var req runRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<16)).Decode(&req); err != nil {
		errJSON(w, http.StatusBadRequest, "bad request: "+err.Error())
		return
	}
	res, err := s.Run(r.Context(), r.PathValue("id"), req)
	if err != nil {
		errJSON(w, http.StatusBadRequest, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, res)
}

func (s *Server) handleExamples(w http.ResponseWriter, r *http.Request) {
	type example struct {
		Name     string `json:"name"`
		Language string `json:"language"`
		Source   string `json:"source"`
	}
	var out []example
	entries, _ := os.ReadDir(s.Examples)
	for _, e := range entries {
		ext := filepath.Ext(e.Name())
		if ext != ".c" && ext != ".wat" {
			continue
		}
		b, err := os.ReadFile(filepath.Join(s.Examples, e.Name()))
		if err != nil {
			continue
		}
		out = append(out, example{Name: e.Name(), Language: strings.TrimPrefix(ext, "."), Source: string(b)})
	}
	writeJSON(w, http.StatusOK, out)
}

func (s *Server) Routes(static string) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/compile", s.handleCompile)
	mux.HandleFunc("POST /api/functions", s.handleUpload)
	mux.HandleFunc("POST /api/functions/{id}/run", s.handleRun)
	mux.HandleFunc("GET /api/examples", s.handleExamples)
	if static != "" {
		mux.Handle("/", http.FileServer(http.Dir(static)))
	}
	return mux
}

func main() {
	exe := ""
	if runtime.GOOS == "windows" {
		exe = ".exe"
	}
	addr := flag.String("addr", "127.0.0.1:8090", "listen address")
	wasp := flag.String("wasp", filepath.Join("..", "runtime", "target", "release", "wasp"+exe), "wasp binary")
	clang := flag.String("clang", "clang", "clang (for C sources)")
	include := flag.String("include", filepath.Join("..", "examples"), "directory with wasi_min.h")
	examples := flag.String("examples", filepath.Join("..", "web", "examples"), "playground examples")
	store := flag.String("store", filepath.Join(os.TempDir(), "wasp-functions"), "module store")
	static := flag.String("static", filepath.Join("..", "web", "dist"), "playground files ('' to disable)")
	parallel := flag.Int("parallel", runtime.NumCPU(), "concurrent runs")
	flag.Parse()
	s := NewServer(*wasp, *clang, *include, *store, *examples, *parallel)
	log.Printf("listening on http://%s (wasp: %s)", *addr, *wasp)
	log.Fatal(http.ListenAndServe(*addr, s.Routes(*static)))
}
