/*
 * A minimal runtime for C programs, with no C library. Compiled for wasm32 it talks to WASI
 * directly (fd_write, clock_time_get, args_get, proc_exit); compiled natively it uses the host's
 * C library, so the same benchmark source runs in Wasp and as a native executable.
 *
 * Provides: _start (calls main with argc/argv), memcpy/memset/memmove, a bump allocator
 * (malloc/calloc/free), xprintf (%d %u %ld %lu %lld %llu %x %s %c %f %%), now_ns.
 */
#ifndef WASI_MIN_H
#define WASI_MIN_H

#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __wasm__

#define WASI(name) __attribute__((import_module("wasi_snapshot_preview1"), import_name(#name)))
WASI(fd_write) int32_t __wasi_fd_write(int32_t fd, const void *iovs, int32_t n, int32_t *written);
WASI(clock_time_get) int32_t __wasi_clock_time_get(int32_t id, int64_t precision, uint64_t *t);
WASI(args_sizes_get) int32_t __wasi_args_sizes_get(int32_t *argc, int32_t *size);
WASI(args_get) int32_t __wasi_args_get(char **argv, char *buf);
WASI(proc_exit) __attribute__((noreturn)) void __wasi_proc_exit(int32_t code);

void *memcpy(void *d, const void *s, size_t n) {
    unsigned char *a = (unsigned char *)d;
    const unsigned char *b = (const unsigned char *)s;
    while (n--) *a++ = *b++;
    return d;
}
void *memmove(void *d, const void *s, size_t n) {
    unsigned char *a = (unsigned char *)d;
    const unsigned char *b = (const unsigned char *)s;
    if (a < b)
        while (n--) *a++ = *b++;
    else
        while (n--) a[n] = b[n];
    return d;
}
void *memset(void *d, int c, size_t n) {
    unsigned char *a = (unsigned char *)d;
    while (n--) *a++ = (unsigned char)c;
    return d;
}

extern unsigned char __heap_base;
static uintptr_t heap_top;

static void *malloc(size_t n) {
    if (!heap_top) heap_top = (uintptr_t)&__heap_base;
    uintptr_t p = (heap_top + 15) & ~(uintptr_t)15;
    uintptr_t end = p + n;
    uintptr_t have = __builtin_wasm_memory_size(0) * 65536u;
    if (end > have) {
        size_t pages = (end - have + 65535) / 65536;
        if (__builtin_wasm_memory_grow(0, pages) == (size_t)-1) return 0;
    }
    heap_top = end;
    return (void *)p;
}
static void free(void *p) { (void)p; }

static void write_out(const char *s, size_t n) {
    struct { const char *p; size_t n; } iov = {s, n};
    int32_t w;
    __wasi_fd_write(1, &iov, 1, &w);
}

static uint64_t now_ns(void) {
    uint64_t t = 0;
    __wasi_clock_time_get(1, 1000, &t);
    return t;
}

int main(int argc, char **argv);

__attribute__((export_name("_start"))) void _start(void) {
    int32_t argc = 0, size = 0;
    __wasi_args_sizes_get(&argc, &size);
    char **argv = (char **)malloc(sizeof(char *) * (size_t)(argc + 1));
    char *buf = (char *)malloc((size_t)size);
    __wasi_args_get(argv, buf);
    argv[argc] = 0;
    __wasi_proc_exit(main(argc, argv));
}

#else /* native */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef _WIN32
#include <windows.h>
static uint64_t now_ns(void) {
    LARGE_INTEGER f, t;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t);
    return (uint64_t)((double)t.QuadPart * 1e9 / (double)f.QuadPart);
}
#else
static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}
#endif
static void write_out(const char *s, size_t n) {
    fwrite(s, 1, n, stdout);
    fflush(stdout);
}

#endif

static void *calloc_z(size_t n, size_t size) {
    unsigned char *p = (unsigned char *)malloc(n * size);
    if (p)
        for (size_t i = 0; i < n * size; i++) p[i] = 0;
    return p;
}

/* ---------------------------------------------------------------- formatted output */

static char *fmt_u(char *end, unsigned long long v, unsigned base) {
    *--end = 0;
    do {
        *--end = "0123456789abcdef"[v % base];
        v /= base;
    } while (v);
    return end;
}

static void xprintf(const char *f, ...) {
    char out[1024];
    size_t n = 0;
    va_list ap;
    va_start(ap, f);
#define PUT(c) do { if (n < sizeof out) out[n++] = (char)(c); } while (0)
    for (; *f; f++) {
        if (*f != '%') {
            PUT(*f);
            continue;
        }
        f++;
        int longs = 0;
        while (*f == 'l') {
            longs++;
            f++;
        }
        char tmp[32], *s;
        switch (*f) {
        case 'd': {
            long long v = longs >= 2 ? va_arg(ap, long long) : longs ? va_arg(ap, long) : va_arg(ap, int);
            if (v < 0) {
                PUT('-');
                v = -v;
            }
            for (s = fmt_u(tmp + 32, (unsigned long long)v, 10); *s; s++) PUT(*s);
            break;
        }
        case 'u':
        case 'x': {
            unsigned long long v = longs >= 2 ? va_arg(ap, unsigned long long) : longs ? va_arg(ap, unsigned long) : va_arg(ap, unsigned);
            for (s = fmt_u(tmp + 32, v, *f == 'x' ? 16 : 10); *s; s++) PUT(*s);
            break;
        }
        case 's':
            for (s = va_arg(ap, char *); *s; s++) PUT(*s);
            break;
        case 'c':
            PUT(va_arg(ap, int));
            break;
        case 'f': {
            double v = va_arg(ap, double);
            if (v < 0) {
                PUT('-');
                v = -v;
            }
            unsigned long long ip = (unsigned long long)v;
            for (s = fmt_u(tmp + 32, ip, 10); *s; s++) PUT(*s);
            PUT('.');
            double frac = v - (double)ip;
            for (int k = 0; k < 6; k++) {
                frac *= 10;
                int d = (int)frac;
                PUT('0' + d);
                frac -= d;
            }
            break;
        }
        case '%':
            PUT('%');
            break;
        default:
            PUT('?');
        }
    }
#undef PUT
    va_end(ap);
    write_out(out, n);
}

#endif
