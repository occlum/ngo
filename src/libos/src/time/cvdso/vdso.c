#include "vdso.h"

vdso_data_t __vdso_data = 0;
u64 LOW_RES_NSEC;

void vdso_init(u64 vdso_data_addr, u64 low_res_nsec) {
    __vdso_data = (vdso_data_t)vdso_data_addr;
    LOW_RES_NSEC = low_res_nsec;
}

vdso_data_t get_vdso_data() {
    return __vdso_data;
}

static inline u32 vdso_read_retry(volatile const struct vdso_data *vd, u32 start) {
    u32 seq;

    smp_rmb();
    seq = ACCESS_ONCE(vd->seq);
    return seq != start;
}

/**
 * rdtsc_ordered() - read the current TSC in program order
 *
 * rdtsc_ordered() returns the result of RDTSC as a 64-bit integer.
 * It is ordered like a load to a global in-memory counter.  It should
 * be impossible to observe non-monotonic rdtsc_unordered() behavior
 * across multiple CPUs as long as the TSC is synced.
 */
static inline unsigned long long rdtsc_ordered(void) {
    DECLARE_ARGS(val, low, high);

    /*
     * The RDTSC instruction is not ordered relative to memory
     * access.  The Intel SDM and the AMD APM are both vague on this
     * point, but empirically an RDTSC instruction can be
     * speculatively executed before prior loads.  An RDTSC
     * immediately after an appropriate barrier appears to be
     * ordered as a normal load, that is, it provides the same
     * ordering guarantees as reading from a global memory location
     * that some other imaginary CPU is updating continuously with a
     * time stamp.
     *
     * Thus, use the preferred barrier on the respective CPU, aiming for
     * RDTSCP as the default.
     */
    __asm__ volatile(ALTERNATIVE_2("rdtsc",
                                   "lfence; rdtsc", X86_FEATURE_LFENCE_RDTSC,
                                   "rdtscp", X86_FEATURE_RDTSCP)
                     : EAX_EDX_RET(val, low, high)
                     /* RDTSCP clobbers ECX with MSR_TSC_AUX. */
                     :: "ecx");

    return EAX_EDX_VAL(val, low, high);
}

/*  only support VDSO_CLOCKMODE_TSC.
	not support VDSO_CLOCKMODE_PVCLOCK or VDSO_CLOCKMODE_HVCLOCK ... */
static inline u64 __arch_get_hw_counter(s32 clock_mode,
                                        volatile const struct vdso_data *vd) {
    return (u64)rdtsc_ordered();
}

static inline u64 vdso_calc_delta(u64 cycles, u64 last, u64 mask, u32 mult) {
    return ((cycles - last) & mask) * mult;
}

static inline u64 vdso_shift_ns(u64 ns, u32 shift) {
    return ns >> shift;
}

static inline u32 __iter_div_u64_rem(u64 dividend, u32 divisor, u64 *remainder) {
    u32 ret = 0;

    while (dividend >= divisor) {
        /* The following asm() prevents the compiler from
           optimising this loop into a modulo operation.  */
        __asm__("" : "+rm"(dividend));

        dividend -= divisor;
        ret++;
    }

    *remainder = dividend;

    return ret;
}

static inline bool vdso_clocksource_ok(volatile const struct vdso_data *vd) {
    return vd->clock_mode != VDSO_CLOCKMODE_NONE;
}

static inline int do_hres(volatile const struct vdso_data *vd, clockid_t clk,
                          struct timespec *ts) {
    volatile const struct vdso_timestamp *vdso_ts = &vd->basetime[clk];
    u64 cycles, last, sec, ns;
    u32 seq;

    do {
        seq = ACCESS_ONCE(vd->seq);

        smp_rmb();

        if (unlikely(!vdso_clocksource_ok(vd))) {
            return -1;
        }

        cycles = __arch_get_hw_counter(vd->clock_mode, vd);

        ns = vdso_ts->nsec;
        last = vd->cycle_last;
        ns += vdso_calc_delta(cycles, last, vd->mask, vd->mult);
        ns = vdso_shift_ns(ns, vd->shift);
        sec = vdso_ts->sec;
    } while (unlikely(vdso_read_retry(vd, seq)));

    /*
     * Do this outside the loop: a race inside the loop could result
     * in __iter_div_u64_rem() being extremely slow.
     */
    ts->tv_sec = sec + __iter_div_u64_rem(ns, NSEC_PER_SEC, &ns);
    ts->tv_nsec = ns;

    return 0;
}

static inline int do_coarse(volatile const struct vdso_data *vd, clockid_t clk,
                            struct timespec *ts) {
    volatile const struct vdso_timestamp *vdso_ts = &vd->basetime[clk];
    u32 seq;

    do {
        seq = ACCESS_ONCE(vd->seq);

        smp_rmb();

        ts->tv_sec = vdso_ts->sec;
        ts->tv_nsec = vdso_ts->nsec;
    } while (unlikely(vdso_read_retry(vd, seq)));

    return 0;
}

int vdso_clock_gettime(clockid_t clock, struct timespec *ts) {
    vdso_data_t vd = get_vdso_data();
    if (!vd) { return -1; }

    switch (clock) {
        case CLOCK_REALTIME:
        case CLOCK_MONOTONIC:
        case CLOCK_BOOTTIME:
            return do_hres(&vd[CS_HRES_COARSE], clock, ts);
        case CLOCK_MONOTONIC_RAW:
            return do_hres(&vd[CS_RAW], clock, ts);
        case CLOCK_REALTIME_COARSE:
        case CLOCK_MONOTONIC_COARSE:
            return do_coarse(&vd[CS_HRES_COARSE], clock, ts);
        default:
            return -1;
    }
}

int vdso_gettimeofday(struct timeval *tv, struct timezone *tz) {
    vdso_data_t vd = get_vdso_data();
    if (!vd) { return -1; }

    if (likely(tv != NULL)) {
        struct timespec ts;

        if (do_hres(&vd[CS_HRES_COARSE], CLOCK_REALTIME, &ts)) {
            return -1;
        }

        tv->tv_sec = ts.tv_sec;
        tv->tv_usec = (u32)ts.tv_nsec / NSEC_PER_USEC;
    }

    if (unlikely(tz != NULL)) {
        tz->tz_minuteswest = vd[CS_HRES_COARSE].tz_minuteswest;
        tz->tz_dsttime = vd[CS_HRES_COARSE].tz_dsttime;
    }

    return 0;
}

time_t vdso_time(time_t *time) {
    vdso_data_t vd = get_vdso_data();
    if (!vd) { return -1; }

    time_t t = ACCESS_ONCE(vd[CS_HRES_COARSE].basetime[CLOCK_REALTIME].sec);

    if (time) {
        *time = t;
    }

    return t;
}

int vdso_clock_getres(clockid_t clock, struct timespec *res) {
    vdso_data_t vd = get_vdso_data();
    if (!vd) { return -1; }

    u64 ns;
    switch (clock) {
        case CLOCK_REALTIME:
        case CLOCK_MONOTONIC:
        case CLOCK_BOOTTIME:
        case CLOCK_MONOTONIC_RAW:
            ns = ACCESS_ONCE(vd[CS_HRES_COARSE].hrtimer_res);
            break;
        case CLOCK_REALTIME_COARSE:
        case CLOCK_MONOTONIC_COARSE:
            // todo: this value should be from
            ns = LOW_RES_NSEC;
            break;
        default:
            return -1;
    }

    if (likely(res)) {
        res->tv_sec = 0;
        res->tv_nsec = ns;
    }
    return 0;
}