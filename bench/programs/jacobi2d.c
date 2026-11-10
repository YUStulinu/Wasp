/* jacobi-2d: five-point stencil over two arrays. */
#include "common.h"
#define N 600
#define T 60
int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    double *A = malloc(8 * N * N), *B = malloc(8 * N * N);
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            A[i * N + j] = ((double)i * (j + 2) + 2) / N;
            B[i * N + j] = ((double)i * (j + 3) + 3) / N;
        }
    uint64_t t0 = now_ns();
    for (int t = 0; t < T; t++) {
        for (int i = 1; i < N - 1; i++)
            for (int j = 1; j < N - 1; j++)
                B[i * N + j] = 0.2 * (A[i * N + j] + A[i * N + j - 1] + A[i * N + j + 1] + A[(i + 1) * N + j] + A[(i - 1) * N + j]);
        for (int i = 1; i < N - 1; i++)
            for (int j = 1; j < N - 1; j++)
                A[i * N + j] = 0.2 * (B[i * N + j] + B[i * N + j - 1] + B[i * N + j + 1] + B[(i + 1) * N + j] + B[(i - 1) * N + j]);
    }
    REPORT("jacobi-2d", dsum(A, N * N), t0);
    return 0;
}
