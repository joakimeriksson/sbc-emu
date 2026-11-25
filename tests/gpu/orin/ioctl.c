// SPDX-License-Identifier: MIT
// Freestanding arm64 Linux probe for the Jetson R36 nvgpu userspace ABI.
#include <stdint.h>

enum {
    SYS_IOCTL = 29,
    SYS_OPENAT = 56,
    SYS_EXIT = 93,
    AT_FDCWD = -100,
    O_RDWR = 2,
};

#define IOCTL_RDWR(type, number, arg) \
    (0xc0000000U | (sizeof(arg) << 16) | ((type) << 8) | (number))

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

static int run_probe(void)
{
    long ctrl = open_device("/dev/nvgpu/igpu0/ctrl");
    if (ctrl < 0)
        return 1;

    uint32_t info[64];
    struct characteristics_query query = { sizeof(info), (uintptr_t)info };
    if (syscall(SYS_IOCTL, ctrl, IOCTL_RDWR('G', 5, query), (long)&query, 0) < 0
        || query.buffer_size < 2 * sizeof(uint32_t) || !info[0] || !info[1])
        return 2;

    struct address_space_args as = { .fd = -1, .va_start = 1ULL << 30, .va_end = 1ULL << 36 };
    if (syscall(SYS_IOCTL, ctrl, IOCTL_RDWR('G', 8, as), (long)&as, 0) < 0 || as.fd < 0)
        return 3;

    int32_t channel = open_device("/dev/nvgpu/igpu0/channel");
    if (channel < 0)
        return 4;

    if (syscall(SYS_IOCTL, as.fd, IOCTL_RDWR('A', 1, channel), (long)&channel, 0) < 0)
        return 5;
    return 0;
}

void _start(void)
{
    syscall(SYS_EXIT, run_probe(), 0, 0, 0);
    __builtin_unreachable();
}
