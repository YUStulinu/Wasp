/* 2mm: D = alpha * A * B * C + beta * D. */
#include "common.h"
#define N 220
int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    double *A = malloc(8 * N * N), *B = malloc(8 * N * N), *C = malloc(8 * N * N), *D = malloc(8 * N * N), *T = malloc(8 * N * N);
    for (int i = 0; i < N * N; i++) {
        A[i] = (double)(i % 97) / 97;
        B[i] = (double)(i % 89) / 89;
        C[i] = (double)(i % 83) / 83;
        D[i] = (double)(i % 79) / 79;
    }
    uint64_t t0 = now_ns();
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            double s = 0;
            for (int k = 0; k < N; k++) s += 1.5 * A[i * N + k] * B[k * N + j];
            T[i * N + j] = s;
        }
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            double s = D[i * N + j] * 1.2;
            for (int k = 0; k < N; k++) s += T[i * N + k] * C[k * N + j];
            D[i * N + j] = s;
        }
    REPORT("2mm", dsum(D, N * N), t0);
    return 0;
}
