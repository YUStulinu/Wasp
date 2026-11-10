// A C program for WASI, built without a C library: the WASI calls are declared directly.
#include "wasi_min.h"

int main(int argc, char **argv) {
    xprintf("hello from C, %d arguments\n", argc);
    for (int i = 0; i < argc; i++) xprintf("  argv[%d] = %s\n", i, argv[i]);
    xprintf("monotonic clock: %u ms\n", (unsigned)(now_ns() / 1000000));
    return 3;
}
