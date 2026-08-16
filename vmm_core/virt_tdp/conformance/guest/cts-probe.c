// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#define _GNU_SOURCE
#include <errno.h>
#include <arpa/inet.h>
#include <math.h>
#include <net/ethernet.h>
#include <net/if.h>
#include <netpacket/packet.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <emmintrin.h>
#include <cpuid.h>

static int cpuid_has(unsigned leaf, unsigned reg, unsigned bit)
{
    unsigned a, b, c, d;
    if (!__get_cpuid(leaf, &a, &b, &c, &d))
        return 0;
    const unsigned values[] = {a, b, c, d};
    return !!(values[reg] & (1U << bit));
}

static uint64_t read_tsc(void)
{
    unsigned low, high;
    __asm__ volatile("lfence; rdtsc" : "=a"(low), "=d"(high) :: "memory");
    return ((uint64_t)high << 32) | low;
}

static void fault_ud(void) { __asm__ volatile("ud2"); }
static void fault_bp(void) { __asm__ volatile("int3"); }
static void fault_de(void)
{
    volatile int zero = 0;
    volatile int one = 1;
    one /= zero;
}
static void fault_read(void)
{
    void *p = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    *(volatile unsigned char *)p;
}
static void fault_write(void)
{
    void *p = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    *(volatile unsigned char *)p = 1;
}
static void fault_nx(void)
{
    unsigned char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 0xc3;
    ((void (*)(void))p)();
}

static int expect_signal(void (*action)(void), int signal)
{
    pid_t child = fork();
    if (child == 0) {
        action();
        _exit(0);
    }
    int status = 0;
    return child < 0 || waitpid(child, &status, 0) != child ||
           !WIFSIGNALED(status) || WTERMSIG(status) != signal;
}

static int architecture_probe(const char *id)
{
    unsigned a, b, c, d;
    if (!strcmp(id, "cpu.cpuid.basic"))
        return !__get_cpuid(0, &a, &b, &c, &d) || a < 1;
    if (!strcmp(id, "cpu.cpuid.vendor")) {
        if (!__get_cpuid(0, &a, &b, &c, &d)) return 1;
        char vendor[13];
        memcpy(vendor, &b, 4); memcpy(vendor + 4, &d, 4); memcpy(vendor + 8, &c, 4);
        vendor[12] = 0;
        return strcmp(vendor, "GenuineIntel") && strcmp(vendor, "AuthenticAMD");
    }
    if (!strcmp(id, "cpu.cpuid.sse2")) return !cpuid_has(1, 3, 26);
    if (!strcmp(id, "cpu.cpuid.xsave")) return !cpuid_has(1, 2, 26);
    if (!strcmp(id, "cpu.cpuid.hypervisor")) {
        int advertised = cpuid_has(1, 2, 31);
        __cpuid(0x40000000, a, b, c, d);
        return advertised && a < 0x40000000;
    }
    if (!strcmp(id, "cpu.cpuid.invariant_tsc"))
        return !__get_cpuid(0x80000007, &a, &b, &c, &d) || !(d & (1U << 8));
    if (!strcmp(id, "cpu.instruction.rdtsc")) {
        uint64_t first = read_tsc();
        for (volatile unsigned i = 0; i < 100000; i++);
        return read_tsc() <= first;
    }
    if (!strcmp(id, "exception.invalid_opcode")) return expect_signal(fault_ud, SIGILL);
    if (!strcmp(id, "exception.breakpoint")) return expect_signal(fault_bp, SIGTRAP);
    if (!strcmp(id, "exception.divide_error")) return expect_signal(fault_de, SIGFPE);
    if (!strcmp(id, "exception.page_fault_read")) return expect_signal(fault_read, SIGSEGV);
    if (!strcmp(id, "exception.page_fault_write")) return expect_signal(fault_write, SIGSEGV);
    if (!strcmp(id, "exception.nx")) return expect_signal(fault_nx, SIGSEGV);
    return 2;
}

static int memory_size_probe(size_t length, int cow)
{
    unsigned char *p = mmap(NULL, length, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return 1;
    for (size_t i = 0; i < length; i += 4096) {
        if (p[i]) return 1;
        p[i] = (unsigned char)(i / 4096 + 17);
    }
    if (cow) {
        pid_t child = fork();
        if (child == 0) {
            for (size_t i = 0; i < length; i += 4096) p[i] ^= 0xff;
            _exit(0);
        }
        int status;
        if (child < 0 || waitpid(child, &status, 0) != child || status) return 1;
        for (size_t i = 0; i < length; i += 4096)
            if (p[i] != (unsigned char)(i / 4096 + 17)) return 1;
    }
    return munmap(p, length) != 0;
}

static int semantic_memory_probe(const char *id)
{
    if (!strcmp(id, "memory.zero.4k")) return memory_size_probe(4096, 0);
    if (!strcmp(id, "memory.zero.2m")) return memory_size_probe(2UL << 20, 0);
    if (!strcmp(id, "memory.zero.64m")) return memory_size_probe(64UL << 20, 0);
    if (!strcmp(id, "memory.cow.4k")) return memory_size_probe(4096, 1);
    if (!strcmp(id, "memory.cow.2m")) return memory_size_probe(2UL << 20, 1);
    if (!strcmp(id, "memory.cow.64m")) return memory_size_probe(64UL << 20, 1);
    if (!strcmp(id, "memory.mprotect.readonly")) return expect_signal(fault_write, SIGSEGV);
    if (!strcmp(id, "memory.mprotect.none")) return expect_signal(fault_read, SIGSEGV);
    if (!strcmp(id, "memory.execute_disable")) return expect_signal(fault_nx, SIGSEGV);
    if (!strcmp(id, "memory.unaligned")) {
        unsigned char p[32] = {0};
        uint64_t value = 0x0123456789abcdefULL, result;
        memcpy(p + 3, &value, sizeof(value)); memcpy(&result, p + 3, sizeof(result));
        return result != value;
    }
    if (!strcmp(id, "memory.map_cycle")) {
        for (unsigned i = 0; i < 1024; i++) {
            volatile uint64_t *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (p == MAP_FAILED || *p != 0) return 1;
            *p = 0xfeed000000000000ULL | i;
            if (*p != (0xfeed000000000000ULL | i) || munmap((void *)p, 4096))
                return 1;
        }
        return 0;
    }
    return 2;
}

static uint64_t nanoseconds(struct timespec value)
{
    return (uint64_t)value.tv_sec * 1000000000ULL + value.tv_nsec;
}

static int clock_advances(clockid_t clock, long sleep_ns)
{
    struct timespec before, after, delay = {
        .tv_sec = sleep_ns / 1000000000L,
        .tv_nsec = sleep_ns % 1000000000L,
    };
    if (clock_gettime(clock, &before) || nanosleep(&delay, NULL) ||
        clock_gettime(clock, &after)) return 1;
    uint64_t elapsed = nanoseconds(after) - nanoseconds(before);
    return elapsed < (uint64_t)sleep_ns || elapsed > 5000000000ULL;
}

static int semantic_time_probe(const char *id)
{
    if (!strcmp(id, "time.clock.monotonic")) return clock_advances(CLOCK_MONOTONIC, 1000000);
    if (!strcmp(id, "time.clock.monotonic_raw")) return clock_advances(CLOCK_MONOTONIC_RAW, 1000000);
    if (!strcmp(id, "time.clock.boottime")) return clock_advances(CLOCK_BOOTTIME, 1000000);
    if (!strcmp(id, "time.clock.realtime")) return clock_advances(CLOCK_REALTIME, 1000000);
    if (!strcmp(id, "time.sleep.1ms")) return clock_advances(CLOCK_MONOTONIC, 1000000);
    if (!strcmp(id, "time.sleep.10ms")) return clock_advances(CLOCK_MONOTONIC, 10000000);
    if (!strcmp(id, "time.sleep.100ms")) return clock_advances(CLOCK_MONOTONIC, 100000000);
    if (!strcmp(id, "time.sleep.1s")) return clock_advances(CLOCK_MONOTONIC, 1000000000);
    if (!strcmp(id, "time.tsc.sleep")) {
        struct timespec delay = {.tv_nsec = 10000000};
        uint64_t before = read_tsc();
        if (nanosleep(&delay, NULL)) return 1;
        return read_tsc() <= before;
    }
    if (!strncmp(id, "time.resolution.", 16)) {
        clockid_t clock = !strcmp(id + 16, "monotonic") ? CLOCK_MONOTONIC :
                          !strcmp(id + 16, "realtime") ? CLOCK_REALTIME :
                          !strcmp(id + 16, "boottime") ? CLOCK_BOOTTIME : -1;
        struct timespec resolution;
        return clock == -1 || clock_getres(clock, &resolution) ||
               nanoseconds(resolution) == 0 || nanoseconds(resolution) > 10000000;
    }
    return 2;
}

struct atomic_context { atomic_uint counter; unsigned iterations; };
static void *atomic_worker(void *opaque)
{
    struct atomic_context *ctx = opaque;
    for (unsigned i = 0; i < ctx->iterations; i++)
        atomic_fetch_add_explicit(&ctx->counter, 1, memory_order_seq_cst);
    return NULL;
}

static int atomic_probe(int cpus, unsigned iterations)
{
    pthread_t *threads = calloc(cpus, sizeof(*threads));
    struct atomic_context ctx = {.counter = 0, .iterations = iterations};
    if (!threads) return 1;
    for (int i = 0; i < cpus; i++)
        if (pthread_create(&threads[i], NULL, atomic_worker, &ctx)) return 1;
    for (int i = 0; i < cpus; i++)
        if (pthread_join(threads[i], NULL)) return 1;
    free(threads);
    return atomic_load(&ctx.counter) != (unsigned)cpus * iterations;
}

static int memory_probe(void)
{
    const size_t length = 256UL << 20;
    unsigned char *p = mmap(NULL, length, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    for (size_t i = 0; i < length; i += 4096) {
        if (p[i] != 0) {
            fprintf(stderr, "nonzero anonymous page at %zu\n", i);
            return 1;
        }
        p[i] = (unsigned char)(i / 4096 + 1);
    }
    pid_t child = fork();
    if (child == 0) {
        for (size_t i = 0; i < length; i += 4096)
            p[i] ^= 0xff;
        _exit(0);
    }
    int status;
    if (child < 0 || waitpid(child, &status, 0) != child || status != 0)
        return 1;
    for (size_t i = 0; i < length; i += 4096) {
        if (p[i] != (unsigned char)(i / 4096 + 1)) {
            fprintf(stderr, "copy-on-write changed parent at %zu\n", i);
            return 1;
        }
    }
    return munmap(p, length) != 0;
}

struct fpu_worker {
    int cpu;
    uint64_t result[2];
};

static void simd_run(uint64_t result[2])
{
    __m128i value = _mm_set_epi64x(0x0123456789abcdefULL,
                                   0xfedcba9876543210ULL);
    const __m128i add = _mm_set_epi64x(0x9e3779b97f4a7c15ULL,
                                      0xd1b54a32d192ed03ULL);
    for (unsigned i = 0; i < 5000000; i++) {
        value = _mm_add_epi64(value, add);
        value = _mm_xor_si128(value, _mm_slli_epi64(value, 13));
        value = _mm_xor_si128(value, _mm_srli_epi64(value, 7));
        if ((i & 0x3fff) == 0)
            sched_yield();
    }
    _mm_storeu_si128((__m128i *)result, value);
}

static void *fpu_worker(void *opaque)
{
    struct fpu_worker *worker = opaque;
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(worker->cpu, &set);
    if (pthread_setaffinity_np(pthread_self(), sizeof(set), &set) != 0)
        return (void *)1;
    simd_run(worker->result);
    static const uint64_t expected[2] = {
        0x6478a700f048679cULL,
        0x323e31dfd1edff46ULL,
    };
    return memcmp(expected, worker->result, sizeof(expected)) == 0 ? NULL : (void *)1;
}

static int fpu_probe(int cpus)
{
    pthread_t *threads = calloc(cpus, sizeof(*threads));
    struct fpu_worker *workers = calloc(cpus, sizeof(*workers));
    if (!threads || !workers)
        return 1;
    for (int cpu = 0; cpu < cpus; cpu++) {
        workers[cpu].cpu = cpu;
        if (pthread_create(&threads[cpu], NULL, fpu_worker, &workers[cpu]) != 0)
            return 1;
    }
    for (int cpu = 0; cpu < cpus; cpu++) {
        void *result;
        if (pthread_join(threads[cpu], &result) != 0 || result != NULL) {
            fprintf(stderr, "SIMD mismatch on CPU%d\n", cpu);
            return 1;
        }
    }
    free(workers);
    free(threads);
    return 0;
}

static int network_tx_probe(const char *interface, int count, size_t frame_size)
{
    unsigned index = if_nametoindex(interface);
    if (!index) {
        perror("if_nametoindex");
        return 1;
    }
    int fd = socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL));
    if (fd < 0) {
        perror("socket(AF_PACKET)");
        return 1;
    }
    struct sockaddr_ll address = {
        .sll_family = AF_PACKET,
        .sll_protocol = htons(ETH_P_ALL),
        .sll_ifindex = (int)index,
        .sll_halen = ETH_ALEN,
    };
    memset(address.sll_addr, 0xff, ETH_ALEN);
    if (frame_size < 64 || frame_size > 1514) {
        fprintf(stderr, "invalid Ethernet frame size %zu\n", frame_size);
        close(fd);
        return 1;
    }
    unsigned char frame[1514] = {0};
    memset(frame, 0xff, ETH_ALEN);
    frame[12] = 0x88;
    frame[13] = 0xb5;
    for (int i = 0; i < count; i++) {
        memcpy(frame + 14, &i, sizeof(i));
        if (sendto(fd, frame, frame_size, 0,
                   (struct sockaddr *)&address, sizeof(address)) != (ssize_t)frame_size) {
            perror("sendto");
            close(fd);
            return 1;
        }
    }
    return close(fd) != 0;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "memory") == 0)
        return memory_probe();
    if (argc == 3 && strcmp(argv[1], "fpu") == 0)
        return fpu_probe(atoi(argv[2]));
    if ((argc == 4 || argc == 5) && strcmp(argv[1], "nettx") == 0)
        return network_tx_probe(argv[2], atoi(argv[3]),
                                argc == 5 ? strtoul(argv[4], NULL, 0) : 128);
    if (argc == 3 && strcmp(argv[1], "fpu-one") == 0) {
        struct fpu_worker worker = {.cpu = atoi(argv[2])};
        return fpu_worker(&worker) != NULL;
    }
    if (argc == 3 && strcmp(argv[1], "semantic") == 0) {
        int result = architecture_probe(argv[2]);
        if (result != 2) return result;
        result = semantic_memory_probe(argv[2]);
        if (result != 2) return result;
        return semantic_time_probe(argv[2]);
    }
    if (argc == 4 && strcmp(argv[1], "atomic") == 0)
        return atomic_probe(atoi(argv[2]), (unsigned)strtoul(argv[3], NULL, 0));
    fprintf(stderr, "usage: %s memory | fpu CPUS | nettx IFACE COUNT | "
                    "semantic ID | atomic CPUS ITERATIONS | fpu-one CPU\n", argv[0]);
    return 2;
}
