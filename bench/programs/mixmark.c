/* A CoreMark-style integer workload written for this project (it is not EEMBC CoreMark):
   linked-list search and reversal, integer matrix multiplication, a state machine over a byte
   string, and CRC-16 over all the results. */
#include "common.h"

typedef struct node {
    struct node *next;
    int16_t data, idx;
} node;

static uint16_t crc16(uint16_t crc, uint8_t b) {
    crc ^= b;
    for (int k = 0; k < 8; k++) crc = crc & 1 ? (uint16_t)((crc >> 1) ^ 0xa001) : (uint16_t)(crc >> 1);
    return crc;
}

static node *reverse(node *l) {
    node *r = 0;
    while (l) {
        node *n = l->next;
        l->next = r;
        r = l;
        l = n;
    }
    return r;
}

static int state_machine(const uint8_t *s, int n) {
    int state = 0, counts[4] = {0, 0, 0, 0};
    for (int i = 0; i < n; i++) {
        uint8_t c = s[i];
        switch (state) {
        case 0: state = (c >= '0' && c <= '9') ? 1 : (c == '+' || c == '-') ? 2 : 3; break;
        case 1: state = (c >= '0' && c <= '9') ? 1 : c == '.' ? 2 : 0; break;
        case 2: state = (c >= '0' && c <= '9') ? 2 : c == 'e' ? 3 : 0; break;
        default: state = c == ',' ? 0 : 3;
        }
        counts[state]++;
    }
    return counts[0] * 3 + counts[1] * 5 + counts[2] * 7 + counts[3];
}

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    enum { LIST = 2000, MAT = 60, STR = 4000 };
    node *nodes = malloc(sizeof(node) * LIST);
    int32_t *A = malloc(4 * MAT * MAT), *B = malloc(4 * MAT * MAT), *C = malloc(4 * MAT * MAT);
    uint8_t *str = malloc(STR);
    const char *alphabet = "0123456789+-.e,ab";
    for (int i = 0; i < STR; i++) str[i] = (uint8_t)alphabet[(i * 7 + i / 3) % 17];
    uint64_t t0 = now_ns();
    uint16_t crc = 0;
    for (int iter = 0; iter < 400; iter++) {
        for (int i = 0; i < LIST; i++) {
            nodes[i].next = i + 1 < LIST ? &nodes[i + 1] : 0;
            nodes[i].data = (int16_t)((i * 37 + iter) & 0x7fff);
            nodes[i].idx = (int16_t)i;
        }
        node *l = reverse(&nodes[0]);
        int found = 0;
        for (int q = 0; q < 40; q++) {
            int16_t want = (int16_t)((q * 911 + iter) & 0x7fff);
            for (node *p = l; p; p = p->next)
                if (p->data == want) {
                    found += p->idx;
                    break;
                }
        }
        crc = crc16(crc, (uint8_t)found);
        for (int i = 0; i < MAT * MAT; i++) {
            A[i] = (i * 13 + iter) % 255 - 127;
            B[i] = (i * 7 + iter * 3) % 251 - 125;
        }
        for (int i = 0; i < MAT; i++)
            for (int j = 0; j < MAT; j++) {
                int32_t s = 0;
                for (int k = 0; k < MAT; k++) s += A[i * MAT + k] * B[k * MAT + j];
                C[i * MAT + j] = s;
            }
        int32_t m = 0;
        for (int i = 0; i < MAT * MAT; i++) m ^= C[i] + (C[i] >> 3);
        crc = crc16(crc, (uint8_t)m);
        crc = crc16(crc, (uint8_t)(m >> 8));
        str[iter % STR] = (uint8_t)alphabet[(iter + m) & 15];
        int sm = state_machine(str, STR);
        crc = crc16(crc, (uint8_t)sm);
        crc = crc16(crc, (uint8_t)(sm >> 8));
    }
    REPORT("mixmark", crc, t0);
    return 0;
}
