/*
 * WASI preview 1 (wasi_snapshot_preview1) for Wasp, in C.
 *
 * The runtime calls wasi_call() with the function number, the guest's linear memory and the raw
 * arguments. Every guest pointer is bounds-checked against that memory. Files are reached only
 * through preopened directories: guest paths are resolved component by component below the
 * directory's host path, and absolute paths or ".." that would leave it fail with ENOTCAPABLE.
 */
#ifdef _WIN32
#define _CRT_RAND_S
#ifndef _CRT_SECURE_NO_WARNINGS
#define _CRT_SECURE_NO_WARNINGS
#endif
#include <direct.h>
#include <io.h>
#include <time.h>
#include <windows.h>
#define stat64_t struct _stat64
#define stat64_fn _stat64
#define fstat64_fn _fstat64
#define fileno_fn _fileno
#define fseek64 _fseeki64
#define ftell64 _ftelli64
#define mkdir_fn(p) _mkdir(p)
#define S_ISDIR(m) (((m) & _S_IFMT) == _S_IFDIR)
#else
#define _GNU_SOURCE
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#define stat64_t struct stat
#define stat64_fn stat
#define fstat64_fn fstat
#define fileno_fn fileno
#define fseek64 fseeko
#define ftell64 ftello
#define mkdir_fn(p) mkdir(p, 0755)
#endif
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "wasi.h"

enum {
    E_SUCCESS = 0, E_ACCES = 2, E_BADF = 8, E_EXIST = 20, E_FAULT = 21, E_INVAL = 28, E_IO = 29,
    E_ISDIR = 31, E_NOENT = 44, E_NOSYS = 52, E_NOTDIR = 54, E_SPIPE = 70, E_NOTCAPABLE = 76,
};

enum { FT_UNKNOWN = 0, FT_CHAR = 2, FT_DIR = 3, FT_FILE = 4 };

#define MAX_FDS 128
#define MAX_PATH_LEN 4096

typedef struct {
    int kind; /* 0 free, 1 stdin, 2 stdout, 3 stderr, 4 file, 5 directory */
    FILE *f;
    char *host_path;
    char *guest_name; /* for preopened directories */
    int preopen;
    int append;
} fd_entry;

struct wasi_ctx {
    char **argv;
    int argc;
    char **env;
    int envc;
    fd_entry fds[MAX_FDS];
    int exit_code;
    char *stdout_buf; /* when capturing: output collected here */
    size_t stdout_len, stdout_cap;
    int capture;
};

static char *dup_str(const char *s) {
    size_t n = strlen(s) + 1;
    char *d = (char *)malloc(n);
    memcpy(d, s, n);
    return d;
}

wasi_ctx *wasi_new(int argc, const char **argv, int envc, const char **env) {
    wasi_ctx *c = (wasi_ctx *)calloc(1, sizeof(wasi_ctx));
    c->argc = argc;
    c->argv = (char **)calloc((size_t)argc + 1, sizeof(char *));
    for (int i = 0; i < argc; i++) c->argv[i] = dup_str(argv[i]);
    c->envc = envc;
    c->env = (char **)calloc((size_t)envc + 1, sizeof(char *));
    for (int i = 0; i < envc; i++) c->env[i] = dup_str(env[i]);
    c->fds[0].kind = 1;
    c->fds[1].kind = 2;
    c->fds[2].kind = 3;
    return c;
}

int wasi_preopen(wasi_ctx *c, const char *host_dir, const char *guest_name) {
    for (int i = 3; i < MAX_FDS; i++) {
        if (c->fds[i].kind == 0) {
            c->fds[i].kind = 5;
            c->fds[i].host_path = dup_str(host_dir);
            c->fds[i].guest_name = dup_str(guest_name);
            c->fds[i].preopen = 1;
            return i;
        }
    }
    return -1;
}

void wasi_free(wasi_ctx *c) {
    for (int i = 0; i < c->argc; i++) free(c->argv[i]);
    for (int i = 0; i < c->envc; i++) free(c->env[i]);
    free(c->argv);
    free(c->env);
    for (int i = 0; i < MAX_FDS; i++) {
        if (c->fds[i].f) fclose(c->fds[i].f);
        free(c->fds[i].host_path);
        free(c->fds[i].guest_name);
    }
    free(c);
}

int wasi_exit_code(const wasi_ctx *c) { return c->exit_code; }

/* ---------------------------------------------------------------- guest memory */

typedef struct {
    uint8_t *mem;
    uint64_t len;
} guest;

static int ok(guest g, uint64_t ptr, uint64_t n) { return ptr <= g.len && n <= g.len - ptr; }
static void put32(guest g, uint64_t p, uint32_t v) { memcpy(g.mem + p, &v, 4); }
static void put64(guest g, uint64_t p, uint64_t v) { memcpy(g.mem + p, &v, 8); }
static uint32_t get32(guest g, uint64_t p) {
    uint32_t v;
    memcpy(&v, g.mem + p, 4);
    return v;
}
static uint64_t get64(guest g, uint64_t p) {
    uint64_t v;
    memcpy(&v, g.mem + p, 8);
    return v;
}

/* ---------------------------------------------------------------- helpers */

static int string_list_sizes(guest g, char **list, int n, uint64_t count_ptr, uint64_t size_ptr) {
    if (!ok(g, count_ptr, 4) || !ok(g, size_ptr, 4)) return E_FAULT;
    uint64_t total = 0;
    for (int i = 0; i < n; i++) total += strlen(list[i]) + 1;
    put32(g, count_ptr, (uint32_t)n);
    put32(g, size_ptr, (uint32_t)total);
    return E_SUCCESS;
}

static int string_list_get(guest g, char **list, int n, uint64_t ptrs, uint64_t buf) {
    if (!ok(g, ptrs, (uint64_t)n * 4)) return E_FAULT;
    for (int i = 0; i < n; i++) {
        size_t len = strlen(list[i]) + 1;
        if (!ok(g, buf, len)) return E_FAULT;
        put32(g, ptrs + 4 * (uint64_t)i, (uint32_t)buf);
        memcpy(g.mem + buf, list[i], len);
        buf += len;
    }
    return E_SUCCESS;
}

static uint64_t now_ns(int monotonic) {
#ifdef _WIN32
    if (monotonic) {
        LARGE_INTEGER f, t;
        QueryPerformanceFrequency(&f);
        QueryPerformanceCounter(&t);
        return (uint64_t)((double)t.QuadPart * 1e9 / (double)f.QuadPart);
    }
    struct timespec ts;
    timespec_get(&ts, TIME_UTC);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
#else
    struct timespec ts;
    clock_gettime(monotonic ? CLOCK_MONOTONIC : CLOCK_REALTIME, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
#endif
}

static void sleep_ns(uint64_t ns) {
#ifdef _WIN32
    Sleep((DWORD)(ns / 1000000));
#else
    struct timespec ts = {(time_t)(ns / 1000000000ull), (long)(ns % 1000000000ull)};
    nanosleep(&ts, NULL);
#endif
}

static fd_entry *get_fd(wasi_ctx *c, uint32_t fd) {
    if (fd >= MAX_FDS || c->fds[fd].kind == 0) return NULL;
    return &c->fds[fd];
}

/* Resolves a guest path below directory fd `dir` into a host path; returns 0 or an errno. */
static int resolve(wasi_ctx *c, guest g, uint32_t dir, uint64_t ptr, uint64_t len, char *out) {
    fd_entry *d = get_fd(c, dir);
    if (!d) return E_BADF;
    if (d->kind != 5) return E_NOTDIR;
    if (!ok(g, ptr, len) || len >= MAX_PATH_LEN) return E_FAULT;
    char path[MAX_PATH_LEN];
    memcpy(path, g.mem + ptr, len);
    path[len] = 0;
    if (memchr(path, 0, len)) return E_INVAL;
    if (path[0] == '/' || strchr(path, '\\') || strchr(path, ':')) return E_NOTCAPABLE;
    /* Normalise component by component; ".." may not climb above the directory. */
    char *parts[512];
    int n = 0;
    for (char *tok = strtok(path, "/"); tok; tok = strtok(NULL, "/")) {
        if (strcmp(tok, ".") == 0) continue;
        if (strcmp(tok, "..") == 0) {
            if (n == 0) return E_NOTCAPABLE;
            n--;
            continue;
        }
        if (n == 512) return E_INVAL;
        parts[n++] = tok;
    }
    size_t used = (size_t)snprintf(out, MAX_PATH_LEN, "%s", d->host_path);
    for (int i = 0; i < n; i++) {
        size_t add = strlen(parts[i]) + 1;
        if (used + add >= MAX_PATH_LEN) return E_INVAL;
        out[used++] = '/';
        memcpy(out + used, parts[i], add);
        used += add - 1;
    }
    return E_SUCCESS;
}

static void write_filestat(guest g, uint64_t p, const stat64_t *st, int filetype) {
    memset(g.mem + p, 0, 64);
    put64(g, p + 0, (uint64_t)st->st_dev);
    put64(g, p + 8, (uint64_t)st->st_ino);
    g.mem[p + 16] = (uint8_t)filetype;
    put64(g, p + 24, (uint64_t)st->st_nlink);
    put64(g, p + 32, (uint64_t)st->st_size);
    put64(g, p + 40, (uint64_t)st->st_atime * 1000000000ull);
    put64(g, p + 48, (uint64_t)st->st_mtime * 1000000000ull);
    put64(g, p + 56, (uint64_t)st->st_ctime * 1000000000ull);
}

static void out_bytes(wasi_ctx *c, int fd, const uint8_t *p, size_t n) {
    if (fd == 2) {
        fwrite(p, 1, n, stderr);
        return;
    }
    if (c->capture) {
        if (c->stdout_len + n > c->stdout_cap) {
            c->stdout_cap = (c->stdout_len + n) * 2;
            c->stdout_buf = (char *)realloc(c->stdout_buf, c->stdout_cap);
        }
        memcpy(c->stdout_buf + c->stdout_len, p, n);
        c->stdout_len += n;
        return;
    }
    fwrite(p, 1, n, stdout);
}

static int random_bytes(uint8_t *p, size_t n) {
#ifdef _WIN32
    for (size_t i = 0; i < n; i += 4) {
        unsigned int r;
        if (rand_s(&r) != 0) return E_IO;
        for (size_t k = 0; k < 4 && i + k < n; k++) p[i + k] = (uint8_t)(r >> (8 * k));
    }
    return E_SUCCESS;
#else
    FILE *f = fopen("/dev/urandom", "rb");
    if (!f) return E_IO;
    size_t got = fread(p, 1, n, f);
    fclose(f);
    return got == n ? E_SUCCESS : E_IO;
#endif
}

/* ---------------------------------------------------------------- the calls */

int32_t wasi_call(wasi_ctx *c, int32_t id, uint8_t *mem, uint64_t mem_len, const uint64_t *a) {
    guest g = {mem, mem_len};
    switch (id) {
    case WASI_ARGS_SIZES_GET: return string_list_sizes(g, c->argv, c->argc, a[0], a[1]);
    case WASI_ARGS_GET: return string_list_get(g, c->argv, c->argc, a[0], a[1]);
    case WASI_ENVIRON_SIZES_GET: return string_list_sizes(g, c->env, c->envc, a[0], a[1]);
    case WASI_ENVIRON_GET: return string_list_get(g, c->env, c->envc, a[0], a[1]);
    case WASI_CLOCK_RES_GET:
        if (!ok(g, a[1], 8)) return E_FAULT;
        put64(g, a[1], 1000);
        return E_SUCCESS;
    case WASI_CLOCK_TIME_GET:
        if (!ok(g, a[2], 8)) return E_FAULT;
        if ((uint32_t)a[0] > 3) return E_INVAL;
        put64(g, a[2], now_ns((uint32_t)a[0] != 0));
        return E_SUCCESS;
    case WASI_FD_WRITE:
    case WASI_FD_READ: {
        uint32_t fd = (uint32_t)a[0];
        uint64_t iovs = (uint32_t)a[1], n = (uint32_t)a[2], result = (uint32_t)a[3];
        fd_entry *e = get_fd(c, fd);
        if (!e) return E_BADF;
        if (!ok(g, iovs, n * 8) || !ok(g, result, 4)) return E_FAULT;
        uint64_t total = 0;
        for (uint64_t i = 0; i < n; i++) {
            uint64_t buf = get32(g, iovs + 8 * i), len = get32(g, iovs + 8 * i + 4);
            if (!ok(g, buf, len)) return E_FAULT;
            if (id == WASI_FD_WRITE) {
                if (e->kind == 2 || e->kind == 3) {
                    out_bytes(c, e->kind == 2 ? 1 : 2, mem + buf, len);
                } else if (e->kind == 4) {
                    if (e->append) fseek64(e->f, 0, SEEK_END);
                    if (fwrite(mem + buf, 1, len, e->f) != len) return E_IO;
                    fflush(e->f);
                } else {
                    return E_BADF;
                }
                total += len;
            } else {
                size_t got;
                if (e->kind == 1) got = fread(mem + buf, 1, len, stdin);
                else if (e->kind == 4) got = fread(mem + buf, 1, len, e->f);
                else return e->kind == 5 ? E_ISDIR : E_BADF;
                total += got;
                if (got < len) break;
            }
        }
        put32(g, result, (uint32_t)total);
        return E_SUCCESS;
    }
    case WASI_FD_SEEK: {
        fd_entry *e = get_fd(c, (uint32_t)a[0]);
        if (!e) return E_BADF;
        if (e->kind != 4) return E_SPIPE;
        if (!ok(g, a[3], 8)) return E_FAULT;
        int whence = (int)(uint8_t)a[2];
        if (whence > 2) return E_INVAL;
        if (fseek64(e->f, (int64_t)a[1], whence == 0 ? SEEK_SET : whence == 1 ? SEEK_CUR : SEEK_END) != 0) return E_INVAL;
        put64(g, a[3], (uint64_t)ftell64(e->f));
        return E_SUCCESS;
    }
    case WASI_FD_CLOSE: {
        uint32_t fd = (uint32_t)a[0];
        fd_entry *e = get_fd(c, fd);
        if (!e) return E_BADF;
        if (e->preopen) return E_NOTCAPABLE;
        if (e->f) fclose(e->f);
        free(e->host_path);
        memset(e, 0, sizeof(*e));
        return E_SUCCESS;
    }
    case WASI_FD_FDSTAT_GET: {
        fd_entry *e = get_fd(c, (uint32_t)a[0]);
        if (!e) return E_BADF;
        if (!ok(g, a[1], 24)) return E_FAULT;
        memset(mem + a[1], 0, 24);
        mem[a[1]] = (uint8_t)(e->kind <= 3 ? FT_CHAR : e->kind == 4 ? FT_FILE : FT_DIR);
        if (e->append) mem[a[1] + 2] = 1;
        put64(g, a[1] + 8, ~0ull);
        put64(g, a[1] + 16, ~0ull);
        return E_SUCCESS;
    }
    case WASI_FD_FDSTAT_SET_FLAGS:
        return get_fd(c, (uint32_t)a[0]) ? E_SUCCESS : E_BADF;
    case WASI_FD_PRESTAT_GET: {
        fd_entry *e = get_fd(c, (uint32_t)a[0]);
        if (!e || !e->preopen) return E_BADF;
        if (!ok(g, a[1], 8)) return E_FAULT;
        memset(mem + a[1], 0, 8);
        put32(g, a[1] + 4, (uint32_t)strlen(e->guest_name));
        return E_SUCCESS;
    }
    case WASI_FD_PRESTAT_DIR_NAME: {
        fd_entry *e = get_fd(c, (uint32_t)a[0]);
        if (!e || !e->preopen) return E_BADF;
        size_t n = strlen(e->guest_name);
        if ((uint32_t)a[2] < n) return E_INVAL;
        if (!ok(g, a[1], n)) return E_FAULT;
        memcpy(mem + a[1], e->guest_name, n);
        return E_SUCCESS;
    }
    case WASI_FD_FILESTAT_GET: {
        fd_entry *e = get_fd(c, (uint32_t)a[0]);
        if (!e) return E_BADF;
        if (!ok(g, a[1], 64)) return E_FAULT;
        stat64_t st;
        memset(&st, 0, sizeof st);
        if (e->kind == 4) {
            fflush(e->f);
            if (fstat64_fn(fileno_fn(e->f), &st) != 0) return E_IO;
            write_filestat(g, a[1], &st, FT_FILE);
        } else if (e->kind == 5) {
            if (stat64_fn(e->host_path, &st) != 0) return E_IO;
            write_filestat(g, a[1], &st, FT_DIR);
        } else {
            write_filestat(g, a[1], &st, FT_CHAR);
        }
        return E_SUCCESS;
    }
    case WASI_PATH_FILESTAT_GET: {
        char host[MAX_PATH_LEN];
        int r = resolve(c, g, (uint32_t)a[0], a[2], a[3], host);
        if (r) return r;
        if (!ok(g, a[4], 64)) return E_FAULT;
        stat64_t st;
        if (stat64_fn(host, &st) != 0) return E_NOENT;
        write_filestat(g, a[4], &st, S_ISDIR(st.st_mode) ? FT_DIR : FT_FILE);
        return E_SUCCESS;
    }
    case WASI_PATH_OPEN: {
        /* path_open(dirfd, dirflags, path, path_len, oflags, rights, rights_inh, fdflags, fd_out) */
        char host[MAX_PATH_LEN];
        int r = resolve(c, g, (uint32_t)a[0], a[2], a[3], host);
        if (r) return r;
        if (!ok(g, a[8], 4)) return E_FAULT;
        uint32_t oflags = (uint32_t)a[4], fdflags = (uint32_t)a[7];
        uint64_t rights = a[5];
        int want_write = (rights & 0x40) != 0 || (oflags & (1 | 8)) != 0;
        int slot = -1;
        for (int i = 3; i < MAX_FDS; i++)
            if (c->fds[i].kind == 0) { slot = i; break; }
        if (slot < 0) return E_IO;
        stat64_t st;
        int exists = stat64_fn(host, &st) == 0;
        if (exists && (oflags & 4) && (oflags & 1)) return E_EXIST;
        if (exists && S_ISDIR(st.st_mode)) {
            if (want_write && (oflags & 8)) return E_ISDIR;
            c->fds[slot].kind = 5;
            c->fds[slot].host_path = dup_str(host);
        } else {
            if (oflags & 2) return exists ? E_NOTDIR : E_NOENT;
            if (!exists && !(oflags & 1)) return E_NOENT;
            const char *mode = (oflags & 8) ? (want_write && (rights & 0x2) ? "w+b" : "wb")
                               : !exists ? "w+b"
                               : want_write ? "r+b" : "rb";
            FILE *f = fopen(host, mode);
            if (!f) return E_ACCES;
            c->fds[slot].kind = 4;
            c->fds[slot].f = f;
            c->fds[slot].host_path = dup_str(host);
            c->fds[slot].append = (fdflags & 1) != 0;
        }
        put32(g, a[8], (uint32_t)slot);
        return E_SUCCESS;
    }
    case WASI_PATH_CREATE_DIRECTORY: {
        char host[MAX_PATH_LEN];
        int r = resolve(c, g, (uint32_t)a[0], a[1], a[2], host);
        if (r) return r;
        return mkdir_fn(host) == 0 ? E_SUCCESS : E_EXIST;
    }
    case WASI_PATH_UNLINK_FILE: {
        char host[MAX_PATH_LEN];
        int r = resolve(c, g, (uint32_t)a[0], a[1], a[2], host);
        if (r) return r;
        return remove(host) == 0 ? E_SUCCESS : E_NOENT;
    }
    case WASI_POLL_ONEOFF: {
        /* Clock subscriptions sleep until the earliest deadline; fd subscriptions are ready. */
        uint64_t in = (uint32_t)a[0], out = (uint32_t)a[1], n = (uint32_t)a[2], nev = (uint32_t)a[3];
        if (n == 0) return E_INVAL;
        if (!ok(g, in, n * 48) || !ok(g, out, n * 32) || !ok(g, nev, 4)) return E_FAULT;
        uint64_t wait = UINT64_MAX;
        int any_fd = 0;
        for (uint64_t i = 0; i < n; i++) {
            uint64_t s = in + 48 * i;
            if (mem[s + 8] == 0) {
                uint64_t t = get64(g, s + 24);
                if (get32(g, s + 40) & 1) { /* absolute */
                    uint64_t now = now_ns(get32(g, s + 16) != 0);
                    t = t > now ? t - now : 0;
                }
                if (t < wait) wait = t;
            } else {
                any_fd = 1;
            }
        }
        if (!any_fd && wait != UINT64_MAX) sleep_ns(wait);
        uint32_t count = 0;
        for (uint64_t i = 0; i < n; i++) {
            uint64_t s = in + 48 * i, e = out + 32 * (uint64_t)count;
            if (mem[s + 8] == 0 && any_fd) continue;
            memset(mem + e, 0, 32);
            put64(g, e, get64(g, s));
            mem[e + 10] = mem[s + 8];
            count++;
        }
        put32(g, nev, count);
        return E_SUCCESS;
    }
    case WASI_RANDOM_GET:
        if (!ok(g, a[0], (uint32_t)a[1])) return E_FAULT;
        return random_bytes(mem + a[0], (uint32_t)a[1]);
    case WASI_SCHED_YIELD: return E_SUCCESS;
    case WASI_PROC_EXIT:
        c->exit_code = (int)(uint32_t)a[0];
        return WASI_EXIT;
    default: return E_NOSYS;
    }
}

void wasi_capture_stdout(wasi_ctx *c, int on) { c->capture = on; }

const char *wasi_captured(const wasi_ctx *c, size_t *len) {
    *len = c->stdout_len;
    return c->stdout_buf ? c->stdout_buf : "";
}
