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
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
#include <emmintrin.h>

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

static int network_tx_probe(const char *interface, int count)
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
    unsigned char frame[128] = {0};
    memset(frame, 0xff, ETH_ALEN);
    frame[12] = 0x88;
    frame[13] = 0xb5;
    for (int i = 0; i < count; i++) {
        memcpy(frame + 14, &i, sizeof(i));
        if (sendto(fd, frame, sizeof(frame), 0,
                   (struct sockaddr *)&address, sizeof(address)) != sizeof(frame)) {
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
    if (argc == 4 && strcmp(argv[1], "nettx") == 0)
        return network_tx_probe(argv[2], atoi(argv[3]));
    fprintf(stderr, "usage: %s memory | fpu CPUS | nettx IFACE COUNT\n", argv[0]);
    return 2;
}
