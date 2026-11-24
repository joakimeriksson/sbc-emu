#include <stdint.h>

enum {
    WIDTH = 640, HEIGHT = 360, STRIDE = 4 * WIDTH,
    ROVER_W = 108, ROVER_H = 48,
    BAR0 = 0x17000000, COPY_RUNLIST = 0x800000, DOORBELL = 0x030090,
    RUNLIST = 0x82000000, USERD = 0x82001000, INSTANCE = 0x82002000,
    PDB = 0x82003000, PTE = 0x82007000, RING = 0x82008000,
    PUSH = 0x82009000, FENCE = 0x8200a000, ROVER = 0x8200b000,
    TERRAIN = 0x82100000, FRAMEBUFFER = 0x8ff00000,
    RING_VA = 0x1000, PUSH_VA = 0x2000, FENCE_VA = 0x3000,
    ROVER_VA = 0x4000, TERRAIN_VA = 0x10000, FRAMEBUFFER_VA = 0x100000,
};

static volatile uint32_t *const commands = (void *)PUSH;

static void put(uint64_t address, uint32_t value) {
    *(volatile uint32_t *)address = value;
}

static uint32_t get(uint64_t address) {
    return *(volatile uint32_t *)address;
}

static void serial(const char *message) {
    put(0x031d0030, 0x301);
    for (; *message; ++message) {
        while (get(0x031d0018) & (1u << 5)) {}
        put(0x031d0000, (uint8_t)*message);
    }
}

static uint64_t ticks(void) {
    uint64_t result;
    __asm__ volatile("mrs %0, cntvct_el0" : "=r"(result));
    return result;
}

static void entry(uint32_t address, uint32_t value) {
    put(address, value);
    put(address + 4, 0);
}

static void map(uint32_t va, uint32_t physical, uint32_t size) {
    for (uint32_t offset = 0; offset < size; offset += 4096)
        entry(PTE + (va + offset) / 4096 * 8, ((physical + offset) >> 4) | 5);
}

static int power_gpu(void) {
    const unsigned hsp = 0x03c00000;
    const unsigned tx = 0x40070000 + 3 * 256, rx = 0x40071000 + 3 * 256;
    unsigned dimension = get(hsp + 0x380);
    unsigned bank = 1 + (dimension & 15) / 2 + (dimension >> 4 & 15) + (dimension >> 8 & 15);
    unsigned doorbell = hsp + bank * 0x10000 + 0x300;
    put(tx + 4, 1); // IVC SYNC
    put(doorbell, 1);
    if (get(rx + 4) != 2)
        return 0;
    put(tx + 4, 0); // IVC ESTABLISHED
    put(doorbell, 1);
    put(tx + 128, 66); // MRQ_PG
    put(tx + 132, 1);  // Reply requested
    put(tx + 136, 1);  // CMD_PG_SET_STATE
    put(tx + 140, 35); // Tegra234 GPU domain
    put(tx + 144, 1);  // PG_STATE_ON
    put(tx, 1);
    put(doorbell, 1);
    if (get(rx) != 1 || get(rx + 128) != 0)
        return 0;
    put(rx + 64, 1);
    return 1;
}

static void init_gpu(void) {
    // All demo VAs are in the first 2 MiB. The small-page pointer is the
    // upper half of the last directory's dual PDE.
    for (uint32_t i = 0; i < 3; ++i)
        entry(PDB + i * 4096, ((PDB + (i + 1) * 4096) >> 4) | 4);
    entry(PDB + 3 * 4096 + 8, (PTE >> 4) | 4);
    map(RING_VA, RING, 4096);
    map(PUSH_VA, PUSH, 4096);
    map(FENCE_VA, FENCE, 4096);
    map(ROVER_VA, ROVER, ROVER_W * ROVER_H * 4);
    map(TERRAIN_VA, TERRAIN, STRIDE * HEIGHT);
    map(FRAMEBUFFER_VA, FRAMEBUFFER, STRIDE * HEIGHT);

    put(INSTANCE + 0x48, RING_VA);
    put(INSTANCE + 0x4c, 2 << 16); // Four GPFIFO entries.
    put(INSTANCE + 0x200, PDB | 2);
    entry(RUNLIST, USERD | 0xa0);
    entry(RUNLIST + 8, INSTANCE); // Channel 0.
    put(BAR0 + 0x12004c, 4);
    put(BAR0 + COPY_RUNLIST + 0x80, RUNLIST | 2);
    put(BAR0 + COPY_RUNLIST + 0x84, 0);
    put(BAR0 + COPY_RUNLIST + 0x88, 1);
}

static void textures(void) {
    for (unsigned y = 0; y < HEIGHT; ++y) {
        for (unsigned x = 0; x < WIDTH; ++x) {
            unsigned hash = (x * 199 + y * 719) ^ (x * y * 17);
            unsigned ridge = 175 + ((x * 3 ^ x / 4) & 31) / 2;
            unsigned ridge2 = 208 + ((x * 5 ^ x / 3) & 31) / 3;
            unsigned color = 0x0e1c36 + (y / 8) * 0x000101;
            if (y < 155 && hash % 997 == 0)
                color = 0xcadde8;
            int dx = (int)x - 493, dy = (int)y - 94;
            if (dx * dx + dy * dy < 43 * 43)
                color = (y & 3) ? 0xf3ad70 : 0xfbc687;
            if (y >= ridge)
                color = 0x253853 + (hash & 3);
            if (y >= ridge2)
                color = 0x36455a + (hash & 7);
            if (y >= 260 && y < 328)
                color = 0x18212d + (hash & 3);
            if (y == 260 || y == 327 || (y == 294 && (x / 29) % 2 == 0))
                color = 0x73939c;
            put(TERRAIN + y * STRIDE + x * 4, color);
        }
    }
    for (unsigned y = 0; y < ROVER_H; ++y) {
        for (unsigned x = 0; x < ROVER_W; ++x) {
            unsigned color = y < 23 ? 0x30baae : 0x176666;
            if (y < 19 && x > 61 && x < 90)
                color = y < 5 ? 0xc1f1df : 0x183a55;
            if (y >= 24 && x >= 96 && y < 30)
                color = 0xffd880;
            if (y == 0 || y == 32 || x == 0 || x == ROVER_W - 1)
                color = 0x103940;
            if (y >= 33)
                color = (y + x) % 5 ? 0x171a23 : 0x5a6770;
            put(ROVER + (y * ROVER_W + x) * 4, color);
        }
    }
}

static unsigned packet(unsigned index, unsigned method, unsigned count) {
    commands[index++] = (1u << 29) | (count << 16) | (method / 4);
    return index;
}

static unsigned blit(unsigned n, uint32_t src, uint32_t dst, unsigned pitch,
                     unsigned width, unsigned height) {
    n = packet(n, 0x400, 8);
    commands[n++] = 0;
    commands[n++] = src;
    commands[n++] = 0;
    commands[n++] = dst;
    commands[n++] = pitch;
    commands[n++] = STRIDE;
    commands[n++] = width * 4;
    commands[n++] = height;
    n = packet(n, 0x300, 1);
    commands[n++] = 0x386; // Both surfaces pitch-linear, multiline, flush.
    return n;
}

static int frame(unsigned number, unsigned slot) {
    unsigned n = packet(0, 0, 1);
    commands[n++] = 0xc7b5;
    n = blit(n, TERRAIN_VA, FRAMEBUFFER_VA, STRIDE, WIDTH, HEIGHT);
    unsigned path = number * 4 % (2 * 500);
    unsigned x = 16 + (path < 500 ? path : 1000 - path);
    uint32_t dst = FRAMEBUFFER_VA + 246 * STRIDE + 4 * x;
    n = blit(n, ROVER_VA, dst, ROVER_W * 4, ROVER_W, 33);
    for (unsigned wheel = 12; wheel < 96; wheel += 61)
        n = blit(n, ROVER_VA + (33 * ROVER_W + wheel) * 4,
                 dst + 33 * STRIDE + wheel * 4, ROVER_W * 4, 24, 15);
    n = packet(n, 0x240, 3);
    commands[n++] = 0;
    commands[n++] = FENCE_VA;
    commands[n++] = number + 1;
    n = packet(n, 0x300, 1);
    commands[n++] = 8;
    put(RING + slot * 8, PUSH_VA);
    put(RING + slot * 8 + 4, n << 10);
    put(USERD + 0x8c, (slot + 1) & 3);
    __asm__ volatile("dsb sy" ::: "memory");
    put(BAR0 + DOORBELL, 0);
    uint64_t start = ticks();
    while (get(FENCE) != number + 1) {
        if (ticks() - start > 31250000)
            return 0;
    }
    if (get(USERD + 0x88) != ((slot + 1) & 3))
        return 0;
    return get(FRAMEBUFFER) == get(TERRAIN) &&
           get(FRAMEBUFFER + dst - FRAMEBUFFER_VA + STRIDE + 4) == get(ROVER + (ROVER_W + 1) * 4);
}

void demo(void) {
    if (!power_gpu()) {
        serial("GA10B_POWER_FAILED\r\n");
        return;
    }
    init_gpu();
    textures();
    for (unsigned n = 0;; ++n) {
        if (!frame(n, n & 3)) {
            serial("GA10B_COPY_FAILED\r\n");
            return;
        }
        if (n == 1)
            serial("GA10B_COPY_DEMO_READY\r\n");
        uint64_t start = ticks();
        while (ticks() - start < 1041666) {}
    }
}
