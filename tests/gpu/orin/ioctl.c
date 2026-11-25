// SPDX-License-Identifier: MIT
// Freestanding arm64 Linux copy and compute test for the Jetson R36 nvgpu ABI.
#include <stdint.h>

enum {
    SYS_IOCTL = 29,
    SYS_OPENAT = 56,
    SYS_PPOLL = 73,
    SYS_EXIT = 93,
    SYS_MMAP = 222,
    AT_FDCWD = -100,
    O_RDWR = 2,
    PROT_READ_WRITE = 3,
    MAP_SHARED = 1,
    PAGE_BYTES = 4096,
    BUFFER_BYTES = 4 * PAGE_BYTES,
    SOURCE_OFFSET = PAGE_BYTES - 4,
    DESTINATION_OFFSET = 3 * PAGE_BYTES - 4,
    SEMAPHORE_OFFSET = 0x300,
    QMD_OFFSET = 0x400,
    SASS_OFFSET = 0x800,
    COMPUTE_INPUT = 0xa00,
    COMPUTE_OUTPUT = 0xa40,
    NVMAP_HEAP_IOVMM = 1U << 30,
    NVMAP_WRITE_COMBINE = 1,
    NVMAP_TEST_TAG = 1U << 16,
    NVGPU_FENCE_GET = 1U << 1,
    NVGPU_FENCE_FD = 1U << 3,
    POLLIN = 1,
};

#define IOCTL(direction, type, number, arg) \
    (((unsigned)(direction) << 30) | (sizeof(arg) << 16) | ((type) << 8) | (number))

enum {
    IOC_WRITE = 1,
    IOC_READ = 2,
    IOC_RDWR = IOC_WRITE | IOC_READ,
};

struct characteristics_query {
    uint64_t buffer_size;
    uint64_t buffer_address;
};

struct address_space_args {
    uint32_t big_page_size;
    int32_t fd;
    uint32_t flags;
    uint32_t reserved;
    uint64_t va_start;
    uint64_t va_end;
    uint64_t reserved_tail[4];
};

_Static_assert(sizeof(struct address_space_args) == 64, "R36 NVGPU_GPU_IOCTL_ALLOC_AS size");

static long syscall(long number, long a, long b, long c, long d)
{
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    register long x8 __asm__("x8") = number;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    return x0;
}

static long open_device(const char *path)
{
    return syscall(SYS_OPENAT, AT_FDCWD, (long)path, O_RDWR, 0);
}

static long mmap_fd(int32_t fd)
{
    register long x0 __asm__("x0") = 0;
    register long x1 __asm__("x1") = BUFFER_BYTES;
    register long x2 __asm__("x2") = PROT_READ_WRITE;
    register long x3 __asm__("x3") = MAP_SHARED;
    register long x4 __asm__("x4") = fd;
    register long x5 __asm__("x5") = 0;
    register long x8 __asm__("x8") = SYS_MMAP;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x8) : "memory");
    return x0;
}

static uint32_t packet(unsigned method, unsigned count)
{
    return (1U << 29) | (count << 16) | (method / 4);
}

static int map_buffer(int as, volatile uint32_t **words, uint64_t *va)
{
    long nvmap = open_device("/dev/nvmap");
    if (nvmap < 0)
        return 9;
    uint32_t create[2] = {BUFFER_BYTES, 0};
    if (syscall(SYS_IOCTL, nvmap, IOCTL(IOC_RDWR, 'N', 0, create), (long)create, 0) < 0)
        return 10;
    uint32_t alloc[5] = {create[1], NVMAP_HEAP_IOVMM, NVMAP_TEST_TAG | NVMAP_WRITE_COMBINE, PAGE_BYTES, 0};
    if (syscall(SYS_IOCTL, nvmap, IOCTL(IOC_WRITE, 'N', 3, alloc), (long)alloc, 0) < 0)
        return 11;
    int32_t get_fd[2] = {-1, create[1]};
    if (syscall(SYS_IOCTL, nvmap, IOCTL(IOC_RDWR, 'N', 0xf, get_fd), (long)get_fd, 0) < 0)
        return 12;
    long addr = mmap_fd(get_fd[0]);
    if (addr < 0)
        return 13;
    struct {
        uint32_t flags;
        int16_t compressible_kind;
        int16_t incompressible_kind;
        int32_t fd;
        uint32_t page_size;
        uint64_t buffer_offset, mapping_size, va;
    } map = { .compressible_kind = -1, .incompressible_kind = 0, .fd = get_fd[0] };
    if (syscall(SYS_IOCTL, as, IOCTL(IOC_RDWR, 'A', 7, map), (long)&map, 0) < 0)
        return 14;

    *words = (volatile uint32_t *)addr;
    *va = map.va;
    return 0;
}

static int submit_work(int channel, uint64_t va, uint32_t count)
{
    __asm__ volatile("dsb sy" ::: "memory");
    const uint32_t entry[2] = {
        va, (va >> 32) | (count << 10)
    };
    struct {
        uint64_t entries;
        uint32_t count, flags, fence_id, fence_value;
    } submit = { (uintptr_t)entry, 1, NVGPU_FENCE_GET | NVGPU_FENCE_FD, 0, 0 };
    long result = syscall(SYS_IOCTL, channel, IOCTL(IOC_RDWR, 'H', 0x6b, submit), (long)&submit, 0);
    if (result < 0)
        return 30 + -result;
    struct {
        int32_t fd;
        int16_t events, revents;
    } pollfd = { submit.fence_id, POLLIN, 0 };
    int64_t timeout[2] = {1, 0};
    if (syscall(SYS_PPOLL, (long)&pollfd, 1, (long)timeout, 0) != 1 || !(pollfd.revents & POLLIN))
        return 17;
    return 0;
}

static int copy_probe(int channel, volatile uint32_t *words, uint64_t va)
{
    const uint64_t source = va + SOURCE_OFFSET;
    const uint64_t dest = va + DESTINATION_OFFSET;
    const uint64_t fence = va + SEMAPHORE_OFFSET;
    words[SOURCE_OFFSET / 4] = 0x1ebad123;
    words[SOURCE_OFFSET / 4 + 1] = 0xa2c4056f;
    words[DESTINATION_OFFSET / 4] = 0;
    words[DESTINATION_OFFSET / 4 + 1] = 0;
    words[SEMAPHORE_OFFSET / 4] = 0;
    // Use the same pitch-linear copy and completion semaphore as the VNC demo.
    const uint32_t push[] = {
        packet(0, 1), 0xc7b5,
        packet(0x400, 8),
        source >> 32, source, dest >> 32, dest, 8, 8, 8, 1,
        packet(0x300, 1), 0x386,
        packet(0x240, 3), fence >> 32, fence, 1,
        packet(0x300, 1), 8,
    };
    for (unsigned i = 0; i < sizeof(push) / sizeof(push[0]); ++i)
        words[i] = push[i];
    int result = submit_work(channel, va, sizeof(push) / sizeof(push[0]));
    if (result)
        return result;
    if (words[SEMAPHORE_OFFSET / 4] != 1)
        return 15;
    if (words[DESTINATION_OFFSET / 4] != 0x1ebad123 || words[DESTINATION_OFFSET / 4 + 1] != 0xa2c4056f)
        return 16;
    return 0;
}

static uint64_t move_imm(unsigned reg, uint32_t value)
{
    return UINT64_C(0x7802) | (uint64_t)reg << 16 | (uint64_t)value << 32;
}

static uint64_t add_registers(unsigned dst, unsigned a, unsigned b)
{
    return UINT64_C(0x7210) | (uint64_t)dst << 16 | (uint64_t)a << 24 | (uint64_t)b << 32;
}

static int compute_probe(int channel, volatile uint32_t *words, uint64_t va)
{
    const uint64_t program = va + SASS_OFFSET;
    const uint64_t output = va + COMPUTE_OUTPUT;
    volatile uint32_t *qmd = words + QMD_OFFSET / 4;
    for (unsigned i = 0; i < 64; ++i)
        qmd[i] = 0;
    qmd[12] = qmd[13] = qmd[14] = 1;
    qmd[18] = (8 << 16) | 0x30; // QMD v3.0, eight threads along X.
    qmd[19] = (1 << 16) | 1;
    qmd[48] = program;
    qmd[49] = program >> 32;

    // SM87 encodings for output[threadIdx.x] = input[threadIdx.x] + 17.
    const uint64_t shader[][2] = {
        {0x0000000000027919, 0x000e220000002100}, // S2R R2, SR_TID.X
        {0x000000040200780c, 0x000fe20003f060f0}, // ISETP.GE.U32.AND P0, PT, R2, 4, PT
        {0x000000000000094d, 0x000fea0003800000}, // @P0 EXIT
        {move_imm(0, output), 0x000fe20000000f00},
        {move_imm(1, output >> 32), 0x000fe20000000f00},
        {add_registers(3, 2, 2), 0x000fe20007ffe0ff},
        {add_registers(3, 3, 3), 0x000fe20007ffe0ff},
        {add_registers(0, 0, 3), 0x000fe20007ffe0ff},
        {0xffffc03f00047981, 0x000fe2000c1ee900}, // LDG.E R4, [R0.64+UZR-0x40]
        {0x0000001104047810, 0x000fe20007ffe0ff}, // IADD3 R4, R4, 17, RZ
        {0x0000000400007986, 0x000fe2000c10e93f}, // STG.E [R0.64+UZR], R4
        {0x000000000000794d, 0x000fea0003800000}, // EXIT
    };
    for (unsigned i = 0; i < sizeof(shader) / sizeof(shader[0]); ++i) {
        for (unsigned half = 0; half < 2; ++half) {
            words[SASS_OFFSET / 4 + 4 * i + 2 * half] = shader[i][half];
            words[SASS_OFFSET / 4 + 4 * i + 2 * half + 1] = shader[i][half] >> 32;
        }
    }
    const uint32_t inputs[] = {3, 42, 0xabcdef01, 0xfffffff8};
    for (unsigned i = 0; i < 4; ++i) {
        words[COMPUTE_INPUT / 4 + i] = inputs[i];
        words[COMPUTE_OUTPUT / 4 + i] = 0;
        words[COMPUTE_OUTPUT / 4 + 4 + i] = 0x9e3779b9;
    }
    words[0] = packet(0, 1);
    words[1] = 0xc7c0;
    words[2] = packet(0x2b4, 1);
    words[3] = (va + QMD_OFFSET) >> 8;
    words[4] = packet(0x2c0, 1);
    words[5] = 3;
    int result = submit_work(channel, va, 6);
    if (result)
        return result;
    for (unsigned i = 0; i < 4; ++i) {
        if (words[COMPUTE_OUTPUT / 4 + i] != inputs[i] + 17
            || words[COMPUTE_OUTPUT / 4 + 4 + i] != 0x9e3779b9)
            return 18;
    }
    return 0;
}

static int run_engine(long ctrl, int compute)
{
    struct address_space_args as = { .fd = -1, .va_start = 1ULL << 30, .va_end = 1ULL << 36 };
    if (syscall(SYS_IOCTL, ctrl, IOCTL(IOC_RDWR, 'G', 8, as), (long)&as, 0) < 0 || as.fd < 0)
        return 3;

    int32_t channel = open_device("/dev/nvgpu/igpu0/channel");
    // OPEN_CHANNEL picks runlist zero, the modeled asynchronous copy engine.
    if (channel < 0)
        return 4;
    if (!compute && (syscall(SYS_IOCTL, channel, IOCTL(IOC_READ, 'H', 0x70, channel), (long)&channel, 0) < 0
                     || channel < 0))
        return 4;

    if (syscall(SYS_IOCTL, as.fd, IOCTL(IOC_RDWR, 'A', 1, channel), (long)&channel, 0) < 0)
        return 5;

    uint64_t tsg[3] = {0};
    if (syscall(SYS_IOCTL, ctrl, IOCTL(IOC_RDWR, 'G', 9, tsg), (long)tsg, 0) < 0)
        return 6;
    const uint32_t tsg_fd = tsg[0];
    if (syscall(SYS_IOCTL, tsg_fd, IOCTL(IOC_WRITE, 'T', 1, channel), (long)&channel, 0) < 0)
        return 7;

    // Allocate 32 GPFIFO entries and reserve 4 in-flight jobs.
    uint32_t gpfifo[8] = {32, 4};
    if (syscall(SYS_IOCTL, channel, IOCTL(IOC_WRITE, 'H', 0x7b, gpfifo), (long)gpfifo, 0) < 0)
        return 8;
    volatile uint32_t *words;
    uint64_t va;
    int result = map_buffer(as.fd, &words, &va);
    if (result)
        return result;
    return compute ? compute_probe(channel, words, va) : copy_probe(channel, words, va);
}

static int run_probe(void)
{
    long ctrl = open_device("/dev/nvgpu/igpu0/ctrl");
    if (ctrl < 0)
        return 1;

    uint32_t info[64];
    struct characteristics_query query = { sizeof(info), (uintptr_t)info };
    if (syscall(SYS_IOCTL, ctrl, IOCTL(IOC_RDWR, 'G', 5, query), (long)&query, 0) < 0
        || query.buffer_size < 2 * sizeof(uint32_t) || !info[0] || !info[1])
        return 2;

    const int copy = run_engine(ctrl, 0);
    if (copy)
        return copy;
    const int compute = run_engine(ctrl, 1);
    return compute ? 100 + compute : 0;
}

void _start(void)
{
    syscall(SYS_EXIT, run_probe(), 0, 0, 0);
    __builtin_unreachable();
}
