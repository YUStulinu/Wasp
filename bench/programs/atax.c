/* atax: y = A^T (A x), repeated. */
#include "common.h"
#define M 1900
#define N 2100
int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    double *A = malloc(8 * (size_t)M * N), *x = malloc(8 * N), *y = malloc(8 * N), *tmp = malloc(8 * M);
    for (int i = 0; i < M; i++)
        for (int j = 0; j < N; j++) A[(size_t)i * N + j] = (double)((i + j) % N) / (5.0 * M);
    for (int j = 0; j < N; j++) x[j] = 1 + (double)j / N;
    uint64_t t0 = now_ns();
    for (int rep = 0; rep < 8; rep++) {
        for (int j = 0; j < N; j++) y[j] = 0;
        for (int i = 0; i < M; i++) {
            double s = 0;
            for (int j = 0; j < N; j++) s += A[(size_t)i * N + j] * x[j];
            tmp[i] = s;
            for (int j = 0; j < N; j++) y[j] += A[(size_t)i * N + j] * s;
        }
        for (int j = 0; j < N; j++) x[j] = y[j] / 1000.0;
    }
    REPORT("atax", dsum(y, N), t0);
    return 0;
}
