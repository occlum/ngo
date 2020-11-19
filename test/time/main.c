#include <sys/time.h>
#include <time.h>
#include "test.h"

// ============================================================================
// Test cases for gettimeofday
// ============================================================================

int test_gettimeofday() {
    struct timeval tv;
    if (gettimeofday(&tv, NULL)) {
        THROW_ERROR("gettimeofday failed");
    }
    return 0;
}

// ============================================================================
// Test cases for clock_gettime
// ============================================================================

int test_clock_gettime() {
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_REALTIME, ...) failed");
    }
    if (clock_gettime(CLOCK_MONOTONIC, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_MONOTONIC, ...) failed");
    }
    return 0;
}

// ============================================================================
// Test cases for clock_getres
// ============================================================================

int test_clock_getres() {
    struct timespec res;
    if (clock_getres(CLOCK_REALTIME, &res)) {
        THROW_ERROR("clock_getres(CLOCK_REALTIME, ...) failed");
    }
    if (clock_getres(CLOCK_MONOTONIC, &res)) {
        THROW_ERROR("clock_getres(CLOCK_MONOTONIC, ...) failed");
    }
    if (clock_getres(CLOCK_MONOTONIC_COARSE, &res)) {
        THROW_ERROR("clock_getres(CLOCK_MONOTONIC_COARSE, ...) failed");
    }
    if (clock_getres(CLOCK_REALTIME, NULL)) {
        THROW_ERROR("clock_getres(CLOCK_REALTIME, NULL) failed");
    }
    return 0;
}

// ============================================================================
// Test cases for vdso
// ============================================================================

int test_vdso() {
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_REALTIME, ...) failed");
    }
    printf("clock_gettime(CLOCK_REALTIME, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           ts.tv_sec, ts.tv_nsec);

    if (clock_gettime(CLOCK_REALTIME_COARSE, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_REALTIME_COARSE, ...) failed");
    }
    printf("clock_gettime(CLOCK_REALTIME_COARSE, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           ts.tv_sec, ts.tv_nsec);

    if (clock_gettime(CLOCK_MONOTONIC, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_MONOTONIC, ...) failed");
    }
    printf("clock_gettime(CLOCK_MONOTONIC, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           ts.tv_sec, ts.tv_nsec);

    if (clock_gettime(CLOCK_MONOTONIC_COARSE, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_MONOTONIC_COARSE, ...) failed");
    }
    printf("clock_gettime(CLOCK_MONOTONIC_COARSE, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           ts.tv_sec, ts.tv_nsec);

    if (clock_gettime(CLOCK_BOOTTIME, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_BOOTTIME, ...) failed");
    }
    printf("clock_gettime(CLOCK_BOOTTIME, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           ts.tv_sec, ts.tv_nsec);

    if (clock_gettime(CLOCK_MONOTONIC_RAW, &ts)) {
        THROW_ERROR("clock_gettime(CLOCK_MONOTONIC_RAW, ...) failed");
    }
    printf("clock_gettime(CLOCK_MONOTONIC_RAW, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           ts.tv_sec, ts.tv_nsec);


    struct timeval tv;
    if (gettimeofday(&tv, NULL)) {
        THROW_ERROR("gettimeofday failed");
    }
    printf("gettimeofday(...) = { .tv_sec = %ld, .tv_usec = %ld}\n", tv.tv_sec, tv.tv_usec);

    struct timespec res;
    if (clock_getres(CLOCK_REALTIME, &res)) {
        THROW_ERROR("clock_getres(CLOCK_REALTIME, ...) failed");
    }
    printf("clock_getres(CLOCK_REALTIME, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           res.tv_sec, res.tv_nsec);

    if (clock_getres(CLOCK_REALTIME_COARSE, &res)) {
        THROW_ERROR("clock_getres(CLOCK_REALTIME_COARSE, ...) failed");
    }
    printf("clock_getres(CLOCK_REALTIME_COARSE, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           res.tv_sec, res.tv_nsec);

    if (clock_getres(CLOCK_MONOTONIC, &res)) {
        THROW_ERROR("clock_getres(CLOCK_MONOTONIC, ...) failed");
    }
    printf("clock_getres(CLOCK_MONOTONIC, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           res.tv_sec, res.tv_nsec);

    if (clock_getres(CLOCK_MONOTONIC_COARSE, &res)) {
        THROW_ERROR("clock_getres(CLOCK_MONOTONIC_COARSE, ...) failed");
    }
    printf("clock_getres(CLOCK_MONOTONIC_COARSE, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           res.tv_sec, res.tv_nsec);

    if (clock_getres(CLOCK_MONOTONIC_RAW, &res)) {
        THROW_ERROR("clock_getres(CLOCK_MONOTONIC_RAW, ...) failed");
    }
    printf("clock_getres(CLOCK_MONOTONIC_RAW, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           res.tv_sec, res.tv_nsec);

    if (clock_getres(CLOCK_BOOTTIME, &res)) {
        THROW_ERROR("clock_getres(CLOCK_BOOTTIME, ...) failed");
    }
    printf("clock_getres(CLOCK_BOOTTIME, ...) = { .tv_sec = %ld, .tv_nsec = %ld}\n",
           res.tv_sec, res.tv_nsec);

    time_t tt;
    time(&tt);
    printf("time(...) = %ld\n", tt);

    return 0;
}

// ============================================================================
// Test suite
// ============================================================================

static test_case_t test_cases[] = {
    TEST_CASE(test_gettimeofday),
    TEST_CASE(test_clock_gettime),
    TEST_CASE(test_clock_getres),
    TEST_CASE(test_vdso),
};

int main() {
    return test_suite_run(test_cases, ARRAY_SIZE(test_cases));
}
