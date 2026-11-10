// The Mandelbrot set in ASCII.
#include "wasi_min.h"

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    const char *shades = " .:-=+*#%@";
    for (int y = 0; y < 24; y++) {
        char line[81];
        for (int x = 0; x < 78; x++) {
            double cr = -2.2 + x * 3.0 / 78, ci = -1.2 + y * 2.4 / 24;
            double zr = 0, zi = 0;
            int k = 0;
            while (k < 200 && zr * zr + zi * zi < 4) {
                double t = zr * zr - zi * zi + cr;
                zi = 2 * zr * zi + ci;
                zr = t;
                k++;
            }
            line[x] = shades[k == 200 ? 9 : k % 9];
        }
        line[78] = '\n';
        line[79] = 0;
        xprintf("%s", line);
    }
    return 0;
}
