/* seidel-2d: nine-point Gauss-Seidel stencil, in place. */
#include "common.h"
#define N 500
#define T 25
int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    double *A = malloc(8 * N * N);
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) A[i * N + j] = ((double)i * (j + 2) + 2) / N;
    uint64_t t0 = now_ns();
    for (int t = 0; t < T; t++)
        for (int i = 1; i < N - 1; i++)
            for (int j = 1; j < N - 1; j++)
                A[i * N + j] = (A[(i - 1) * N + j - 1] + A[(i - 1) * N + j] + A[(i - 1) * N + j + 1] + A[i * N + j - 1] + A[i * N + j] +
                                A[i * N + j + 1] + A[(i + 1) * N + j - 1] + A[(i + 1) * N + j] + A[(i + 1) * N + j + 1]) /
                               9.0;
    REPORT("seidel-2d", dsum(A, N * N), t0);
    return 0;
}
