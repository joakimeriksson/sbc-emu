// SPDX-License-Identifier: MIT
// Freestanding arm64 Linux copy-engine test for the Jetson R36 nvgpu ABI.
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

static int copy_probe(int as, int channel)
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

    const uint64_t source = map.va + SOURCE_OFFSET;
    const uint64_t dest = map.va + DESTINATION_OFFSET;
    const uint64_t fence = map.va + SEMAPHORE_OFFSET;
    volatile uint32_t *words = (volatile uint32_t *)addr;
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
    __asm__ volatile("dsb sy" ::: "memory");
    const uint32_t entry[2] = {
        map.va, (map.va >> 32) | ((sizeof(push) / 4) << 10)
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
    if (words[SEMAPHORE_OFFSET / 4] != 1)
        return 15;
    if (words[DESTINATION_OFFSET / 4] != 0x1ebad123 || words[DESTINATION_OFFSET / 4 + 1] != 0xa2c4056f)
        return 16;
    return 0;
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

    struct address_space_args as = { .fd = -1, .va_start = 1ULL << 30, .va_end = 1ULL << 36 };
    if (syscall(SYS_IOCTL, ctrl, IOCTL(IOC_RDWR, 'G', 8, as), (long)&as, 0) < 0 || as.fd < 0)
        return 3;

    long channel_device = open_device("/dev/nvgpu/igpu0/channel");
    int32_t channel = -1;
    // OPEN_CHANNEL picks runlist zero, the modeled asynchronous copy engine.
    if (channel_device < 0
        || syscall(SYS_IOCTL, channel_device, IOCTL(IOC_READ, 'H', 0x70, channel), (long)&channel, 0) < 0
        || channel < 0)
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
    return copy_probe(as.fd, channel);
}

void _start(void)
{
    syscall(SYS_EXIT, run_probe(), 0, 0, 0);
    __builtin_unreachable();
}
