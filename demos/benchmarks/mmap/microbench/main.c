#define _GNU_SOURCE
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <errno.h>
#include <assert.h>
#include <string.h>
#include <fcntl.h>
#include <sys/time.h>

// ============================================================================
// Helper macros
// ============================================================================

#define KB                      (1024UL)
#define MB                      (1024 * 1024UL)
#define PAGE_SIZE               (4 * KB)

#define ALIGN_DOWN(x, a)        ((x) & ~(a-1)) // a must be a power of two
#define ALIGN_UP(x, a)          ALIGN_DOWN((x+(a-1)), (a))
#define ARRAY_SIZE(array)   (sizeof(array)/sizeof(array[0]))

#define MAX_MMAP_USED_MEMORY    (1600 * MB)
#define MAX_BUF_NUM             100000
#define REPEAT_TIMES            1
// ============================================================================
// Helper functions
// ============================================================================
static int check_bytes_in_buf(char *buf, size_t len, int expected_byte_val) {
    for (size_t bi = 0; bi < len; bi++) {
        if (buf[bi] != (char)expected_byte_val) {
            printf("check_bytes_in_buf: expect %02X, but found %02X, at offset %lu\n",
                   (unsigned char)expected_byte_val, (unsigned char)buf[bi], bi);
            return -1;
        }
    }
    return 0;
}

// ============================================================================
// Main functions
// ============================================================================
int main() {
    int prot = PROT_READ | PROT_WRITE;
    int flags = MAP_PRIVATE | MAP_ANONYMOUS;

    void *bufs[MAX_BUF_NUM] = {NULL};
    size_t lens[MAX_BUF_NUM];
    size_t num_bufs = 0;
    size_t used_memory = 0;

    struct timeval time_begin, time_end1, time_end2;
    unsigned long mmap_time = 0, munmap_time = 0;

    // Phrase 1: do mmap with random sizes until no more buffers or memory
    gettimeofday(&time_begin, NULL);
    for (num_bufs = 0;
            num_bufs < ARRAY_SIZE(bufs) && used_memory < MAX_MMAP_USED_MEMORY;
            num_bufs++) {
        // Choose the mmap size randomly but no bigger than 128 KB because if the size is
        // too big, the mmap time will be very small.
        size_t len = rand() % (128 * KB) + 1;
        len = ALIGN_UP(len, PAGE_SIZE);

        // Do mmap
        void *buf = mmap(NULL, len, prot, flags, -1, 0);
        if (buf == MAP_FAILED) {
            printf("mmap failed\n");
            printf("used_memory = %ld, mmap time = %ld\n", used_memory, num_bufs);
            return -1;
        }
        bufs[num_bufs] = buf;
        lens[num_bufs] = len;

        // Update memory usage
        used_memory += len;
        // check_bytes_in_buf(buf, len, 0);
    }
    gettimeofday(&time_end1, NULL);

    // Phrase 2: do munmap to free all memory mapped memory
    for (int bi = 0; bi < num_bufs; bi++) {
        void *buf = bufs[bi];
        size_t len = lens[bi];
        int ret = munmap(buf, len);
        if (ret < 0) {
            printf("munmap failed");
            return -1;
        }

        bufs[bi] = NULL;
        lens[bi] = 0;
    }
    gettimeofday(&time_end2, NULL);
    mmap_time += (time_end1.tv_sec - time_begin.tv_sec) * 1000000
                            + (time_end1.tv_usec - time_begin.tv_usec);
    munmap_time += (time_end2.tv_sec - time_end1.tv_sec) * 1000000
                            + (time_end2.tv_usec - time_end1.tv_usec);

    num_bufs = 0;
    used_memory = 0;

    printf("Done.\n");
    printf("mmap time = %lu us, munmap time = %lu us\n", mmap_time, munmap_time);
    return 0;
}

