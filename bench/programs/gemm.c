/* gemm: C = alpha * A * B + beta * C, n x n doubles. */
#include "common.h"
#define N 300
int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    double *A = malloc(sizeof(double) * N * N), *B = malloc(sizeof(double) * N * N), *C = malloc(sizeof(double) * N * N);
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            A[i * N + j] = (double)((i * j + 1) % N) / N;
            B[i * N + j] = (double)((i * (j + 1) + 2) % N) / N;
            C[i * N + j] = (double)((i * (j + 2) + 3) % N) / N;
        }
    uint64_t t0 = now_ns();
    double alpha = 1.5, beta = 1.2;
    for (int i = 0; i < N; i++) {
        for (int j = 0; j < N; j++) C[i * N + j] *= beta;
        for (int k = 0; k < N; k++) {
            double a = alpha * A[i * N + k];
            for (int j = 0; j < N; j++) C[i * N + j] += a * B[k * N + j];
        }
    }
    REPORT("gemm", dsum(C, N * N), t0);
    return 0;
}
