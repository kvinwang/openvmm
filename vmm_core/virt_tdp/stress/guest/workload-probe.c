// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <net/ethernet.h>
#include <net/if.h>
#include <netpacket/packet.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static uint64_t now_ns(void)
{
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value)) abort();
    return (uint64_t)value.tv_sec * 1000000000ULL + value.tv_nsec;
}

struct cpu_work { int cpu; unsigned seconds; uint64_t iterations; uint64_t digest; };
static void *cpu_worker(void *opaque)
{
    struct cpu_work *work = opaque;
    cpu_set_t set;
    CPU_ZERO(&set); CPU_SET(work->cpu, &set);
    if (pthread_setaffinity_np(pthread_self(), sizeof(set), &set)) return (void *)1;
    uint64_t x = 0x9e3779b97f4a7c15ULL ^ (unsigned)work->cpu;
    uint64_t deadline = now_ns() + (uint64_t)work->seconds * 1000000000ULL;
    do {
        for (unsigned i = 0; i < 100000; i++) {
            x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
            x *= 0x2545f4914f6cdd1dULL;
        }
        work->iterations += 100000;
    } while (now_ns() < deadline);
    work->digest = x;
    return NULL;
}

static int cpu_test(int threads, unsigned seconds)
{
    pthread_t *ids = calloc(threads, sizeof(*ids));
    struct cpu_work *work = calloc(threads, sizeof(*work));
    if (!ids || !work) return 1;
    uint64_t start = now_ns(), total = 0, digest = 0;
    for (int i = 0; i < threads; i++) {
        work[i].cpu = i; work[i].seconds = seconds;
        if (pthread_create(&ids[i], NULL, cpu_worker, &work[i])) return 1;
    }
    for (int i = 0; i < threads; i++) {
        void *result;
        if (pthread_join(ids[i], &result) || result) return 1;
        total += work[i].iterations; digest ^= work[i].digest;
    }
    double elapsed = (now_ns() - start) / 1e9;
    printf("iterations=%llu iterations_per_second=%.0f digest=%016llx\n",
           (unsigned long long)total, total / elapsed, (unsigned long long)digest);
    return total == 0;
}

struct memory_work { unsigned char *memory; size_t begin, end; unsigned passes; uint64_t checksum; };
static void *memory_worker(void *opaque)
{
    struct memory_work *work = opaque;
    for (unsigned pass = 0; pass < work->passes; pass++) {
        unsigned char value = (unsigned char)(pass * 29 + 7);
        memset(work->memory + work->begin, value, work->end - work->begin);
        uint64_t sum = 0;
        for (size_t i = work->begin; i < work->end; i += 4096) sum += work->memory[i];
        work->checksum += sum;
    }
    return NULL;
}

static int memory_test(int threads, size_t mib, unsigned passes)
{
    size_t bytes = mib << 20;
    unsigned char *memory;
    if (posix_memalign((void **)&memory, 4096, bytes)) return 1;
    pthread_t *ids = calloc(threads, sizeof(*ids));
    struct memory_work *work = calloc(threads, sizeof(*work));
    if (!ids || !work) return 1;
    uint64_t start = now_ns(), checksum = 0;
    for (int i = 0; i < threads; i++) {
        work[i] = (struct memory_work){memory, bytes * i / threads,
            bytes * (i + 1) / threads, passes, 0};
        if (pthread_create(&ids[i], NULL, memory_worker, &work[i])) return 1;
    }
    for (int i = 0; i < threads; i++) {
        if (pthread_join(ids[i], NULL)) return 1;
        checksum += work[i].checksum;
    }
    double elapsed = (now_ns() - start) / 1e9;
    printf("bytes=%zu passes=%u mib_per_second=%.1f checksum=%llu\n",
           bytes, passes, mib * (double)passes / elapsed, (unsigned long long)checksum);
    free(work); free(ids); free(memory);
    return checksum == 0;
}

static int process_test(unsigned count)
{
    uint64_t start = now_ns();
    for (unsigned i = 0; i < count; i++) {
        pid_t child = fork();
        if (child == 0) _exit((int)(i & 0x7f));
        int status;
        if (child < 0 || waitpid(child, &status, 0) != child ||
            !WIFEXITED(status) || WEXITSTATUS(status) != (int)(i & 0x7f)) return 1;
    }
    double elapsed = (now_ns() - start) / 1e9;
    printf("processes=%u processes_per_second=%.1f\n", count, count / elapsed);
    return 0;
}

static int network_test(const char *interface, unsigned count, size_t size)
{
    unsigned index = if_nametoindex(interface);
    int fd = socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL));
    if (!index || fd < 0 || size < 64 || size > 1514) return 1;
    struct sockaddr_ll address = {.sll_family = AF_PACKET, .sll_protocol = htons(ETH_P_ALL),
        .sll_ifindex = (int)index, .sll_halen = ETH_ALEN};
    memset(address.sll_addr, 0xff, ETH_ALEN);
    unsigned char frame[1514] = {0};
    memset(frame, 0xff, ETH_ALEN); frame[12] = 0x88; frame[13] = 0xb5;
    uint64_t start = now_ns();
    for (unsigned i = 0; i < count; i++) {
        memcpy(frame + 14, &i, sizeof(i));
        if (sendto(fd, frame, size, 0, (struct sockaddr *)&address, sizeof(address)) !=
            (ssize_t)size) return 1;
    }
    double elapsed = (now_ns() - start) / 1e9;
    printf("packets=%u bytes=%llu packets_per_second=%.1f mib_per_second=%.1f\n",
           count, (unsigned long long)count * size, count / elapsed,
           count * size / elapsed / 1048576.0);
    return close(fd) != 0;
}

int main(int argc, char **argv)
{
    if (argc == 4 && !strcmp(argv[1], "cpu")) return cpu_test(atoi(argv[2]), atoi(argv[3]));
    if (argc == 5 && !strcmp(argv[1], "memory"))
        return memory_test(atoi(argv[2]), strtoul(argv[3], NULL, 0), atoi(argv[4]));
    if (argc == 3 && !strcmp(argv[1], "process")) return process_test(atoi(argv[2]));
    if (argc == 5 && !strcmp(argv[1], "network"))
        return network_test(argv[2], strtoul(argv[3], NULL, 0), strtoul(argv[4], NULL, 0));
    fprintf(stderr, "usage: %s cpu THREADS SECONDS | memory THREADS MIB PASSES | "
                    "process COUNT | network IFACE COUNT SIZE\n", argv[0]);
    return 2;
}
