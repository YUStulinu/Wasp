/* floyd-warshall: all-pairs shortest paths on an integer graph. */
#include "common.h"
#define N 400
int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    int *d = malloc(4 * N * N);
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            d[i * N + j] = i * j % 7 + 1;
            if ((i + j) % 13 == 0 || (i + j) % 7 == 0 || (i + j) % 11 == 0) d[i * N + j] = 999;
        }
    uint64_t t0 = now_ns();
    for (int k = 0; k < N; k++)
        for (int i = 0; i < N; i++)
            for (int j = 0; j < N; j++) {
                int via = d[i * N + k] + d[k * N + j];
                if (via < d[i * N + j]) d[i * N + j] = via;
            }
    uint64_t s = 0;
    for (int i = 0; i < N * N; i++) s += (uint64_t)d[i] * (uint64_t)(i % 31 + 1);
    REPORT("floyd-warshall", s, t0);
    return 0;
}
