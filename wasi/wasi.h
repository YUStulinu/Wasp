/* WASI preview 1 for Wasp: one entry point taking the function number and raw arguments. */
#ifndef WASP_WASI_H
#define WASP_WASI_H

#include <stddef.h>
#include <stdint.h>

typedef struct wasi_ctx wasi_ctx;

/* Function numbers (must match runtime/src/wasi.rs). */
enum {
    WASI_ARGS_GET = 0,
    WASI_ARGS_SIZES_GET,
    WASI_ENVIRON_GET,
    WASI_ENVIRON_SIZES_GET,
    WASI_CLOCK_RES_GET,
    WASI_CLOCK_TIME_GET,
    WASI_FD_CLOSE,
    WASI_FD_FDSTAT_GET,
    WASI_FD_FDSTAT_SET_FLAGS,
    WASI_FD_FILESTAT_GET,
    WASI_FD_PRESTAT_GET,
    WASI_FD_PRESTAT_DIR_NAME,
    WASI_FD_READ,
    WASI_FD_SEEK,
    WASI_FD_WRITE,
    WASI_PATH_CREATE_DIRECTORY,
    WASI_PATH_FILESTAT_GET,
    WASI_PATH_OPEN,
    WASI_PATH_UNLINK_FILE,
    WASI_POLL_ONEOFF,
    WASI_PROC_EXIT,
    WASI_RANDOM_GET,
    WASI_SCHED_YIELD,
    WASI_FD_READDIR,
    WASI_FD_TELL,
    WASI_FD_SYNC,
    WASI_COUNT
};

/* Returned by wasi_call when the guest called proc_exit. */
#define WASI_EXIT (-1)

wasi_ctx *wasi_new(int argc, const char **argv, int envc, const char **env);
int wasi_preopen(wasi_ctx *c, const char *host_dir, const char *guest_name);
void wasi_free(wasi_ctx *c);
int32_t wasi_call(wasi_ctx *c, int32_t id, uint8_t *mem, uint64_t mem_len, const uint64_t *args);
int wasi_exit_code(const wasi_ctx *c);
void wasi_capture_stdout(wasi_ctx *c, int on);
const char *wasi_captured(const wasi_ctx *c, size_t *len);

#endif
