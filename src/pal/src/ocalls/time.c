#include <pthread.h>
#include <sys/time.h>
#include <sys/prctl.h>
#include <sys/auxv.h>
#include "ocalls.h"

void occlum_ocall_gettimeofday(struct timeval *tv) {
    gettimeofday(tv, NULL);
}

void occlum_ocall_clock_gettime(int clockid, struct timespec *tp) {
    clock_gettime(clockid, tp);
}

void occlum_ocall_clock_getres(int clockid, struct timespec *res) {
    clock_getres(clockid, res);
}

int occlum_ocall_nanosleep(const struct timespec *req, struct timespec *rem) {
    return nanosleep(req, rem);
}

int occlum_ocall_thread_getcpuclock(struct timespec *tp) {
    clockid_t thread_clock_id;
    int ret = pthread_getcpuclockid(pthread_self(), &thread_clock_id);
    if (ret != 0) {
        PAL_ERROR("failed to get clock id");
        return -1;
    }

    return clock_gettime(thread_clock_id, tp);
}

void occlum_ocall_rdtsc(uint32_t *low, uint32_t *high) {
    uint64_t rax, rdx;
    asm volatile("rdtsc" : "=a"(rax), "=d"(rdx));
    *low = (uint32_t)rax;
    *high = (uint32_t)rdx;
}

void occlum_ocall_get_timerslack(int *timer_slack) {
    int nanoseconds = prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0);
    *timer_slack = nanoseconds;
}

uint64_t occlum_ocall_get_vdso_info(uint64_t *low_res_nsec) {
    struct timespec tv;
    if (!clock_getres(CLOCK_REALTIME_COARSE, &tv)) {
        *low_res_nsec = tv.tv_nsec;
    }

    uint64_t vdso_addr = getauxval(AT_SYSINFO_EHDR);

    int vvar_offset = -4;
    // todo: determine the vvar_offset, maybe -1, -2, -3, -4, -5...

    // return __vdso_data addr
    return (vdso_addr + vvar_offset * 4096) + 128;
}