/* n-body: leapfrog integration of 5 bodies in double precision. */
#include "common.h"
typedef struct {
    double x, y, z, vx, vy, vz, m;
} body;
int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    body b[5] = {
        {0, 0, 0, 0, 0, 0, 39.47841760435743},
        {4.84, -1.16, -0.10, 0.606, 2.81, -0.025, 0.0377},
        {8.34, 4.12, -0.40, -1.01, 1.82, 0.008, 0.0113},
        {12.89, -15.11, -0.22, 1.08, 0.868, -0.010, 0.0017},
        {15.37, -25.91, 0.17, 0.979, 0.594, -0.034, 0.0020},
    };
    uint64_t t0 = now_ns();
    double dt = 0.01;
    for (int step = 0; step < 3000000; step++) {
        for (int i = 0; i < 5; i++)
            for (int j = i + 1; j < 5; j++) {
                double dx = b[i].x - b[j].x, dy = b[i].y - b[j].y, dz = b[i].z - b[j].z;
                double d2 = dx * dx + dy * dy + dz * dz;
                double mag = dt / (d2 * __builtin_sqrt(d2));
                b[i].vx -= dx * b[j].m * mag;
                b[i].vy -= dy * b[j].m * mag;
                b[i].vz -= dz * b[j].m * mag;
                b[j].vx += dx * b[i].m * mag;
                b[j].vy += dy * b[i].m * mag;
                b[j].vz += dz * b[i].m * mag;
            }
        for (int i = 0; i < 5; i++) {
            b[i].x += dt * b[i].vx;
            b[i].y += dt * b[i].vy;
            b[i].z += dt * b[i].vz;
        }
    }
    double e = 0;
    for (int i = 0; i < 5; i++) e += b[i].x + b[i].y + b[i].z;
    REPORT("nbody", (uint64_t)(int64_t)(e * 1e6), t0);
    return 0;
}
