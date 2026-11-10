/* Shared by the benchmark programs: timing and the result line the harness parses. */
#include "../../examples/wasi_min.h"

#define REPORT(name, checksum, t0) \
    xprintf("%s checksum=%llu ns=%llu\n", name, (unsigned long long)(checksum), (unsigned long long)(now_ns() - (t0)))

/* Checksum of a double array: the sum, scaled and truncated. Identical across builds because
   every build computes with IEEE doubles and no contraction into fused multiply-adds. */
static uint64_t dsum(const double *x, int n) {
    double s = 0;
    for (int i = 0; i < n; i++) s += x[i];
    return (uint64_t)(int64_t)(s * 1000.0);
}
