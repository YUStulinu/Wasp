// Counts primes below a limit (the first argument, default 2,000,000) with a sieve.
#include "wasi_min.h"

int main(int argc, char **argv) {
    unsigned n = 2000000;
    if (argc > 1) {
        n = 0;
        for (char *p = argv[1]; *p >= '0' && *p <= '9'; p++) n = n * 10 + (unsigned)(*p - '0');
    }
    unsigned char *composite = calloc_z(n + 1, 1);
    uint64_t t0 = now_ns();
    unsigned count = 0;
    for (unsigned i = 2; i <= n; i++) {
        if (composite[i]) continue;
        count++;
        for (unsigned long long j = (unsigned long long)i * i; j <= n; j += i) composite[j] = 1;
    }
    xprintf("%u primes up to %u (%u ms)\n", count, n, (unsigned)((now_ns() - t0) / 1000000));
    return 0;
}
