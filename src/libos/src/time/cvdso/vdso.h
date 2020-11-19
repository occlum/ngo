#ifndef VDSO_H
#define VDSO_H

#include "compiler.h"

typedef signed char int8_t;
typedef unsigned char uint8_t;
typedef signed short int int16_t;
typedef unsigned short int uint16_t;
typedef signed int int32_t;
typedef unsigned int uint32_t;
typedef signed long int int64_t;
typedef unsigned long int uint64_t;

typedef uint64_t u64;
typedef int64_t s64;
typedef uint32_t u32;
typedef int32_t s32;

typedef long time_t;
typedef long suseconds_t;
typedef int clockid_t;

#define bool int
#define true 1
#define false 0

#define NULL 0

#define PAGE_SIZE 4096

struct timespec {
	time_t tv_sec;		/* seconds */
	long tv_nsec;	/* nanoseconds */
};

struct timeval {
    time_t      tv_sec;     /* seconds */
    suseconds_t tv_usec;    /* microseconds */
};

// sys/time.h
struct timezone
{
    int tz_minuteswest;		/* Minutes west of GMT.  */
    int tz_dsttime;		/* Nonzero if DST is ever in effect.  */
};

// vdso/clocksource.h
enum vdso_clock_mode {
	VDSO_CLOCKMODE_NONE,
};

// linux/time.h
#define CLOCK_REALTIME			0
#define CLOCK_MONOTONIC			1
#define CLOCK_PROCESS_CPUTIME_ID	2
#define CLOCK_THREAD_CPUTIME_ID		3
#define CLOCK_MONOTONIC_RAW		4
#define CLOCK_REALTIME_COARSE		5
#define CLOCK_MONOTONIC_COARSE		6
#define CLOCK_BOOTTIME			7

/*
 * The driver implementing this got removed. The clock ID is kept as a
 * place holder. Do not reuse!
 */
#define CLOCK_SGI_CYCLE			10
#define CLOCK_TAI			11

#define MAX_CLOCKS			16
#define CLOCKS_MASK			(CLOCK_REALTIME | CLOCK_MONOTONIC)
#define CLOCKS_MONO			CLOCK_MONOTONIC

// vdso/time64.h
#define MSEC_PER_SEC	1000L
#define USEC_PER_MSEC	1000L
#define NSEC_PER_USEC	1000L
#define NSEC_PER_MSEC	1000000L
#define USEC_PER_SEC	1000000L
#define NSEC_PER_SEC	1000000000L
#define FSEC_PER_SEC	1000000000000000LL

// vdso/ktime.h
// #define HZ 100
// #define TICK_NSEC ((NSEC_PER_SEC+HZ/2)/HZ)
// #define LOW_RES_NSEC		TICK_NSEC

// vdso/datapage.h
struct arch_vdso_data {};

#define VDSO_BASES	(CLOCK_TAI + 1)

#define CS_HRES_COARSE	0
#define CS_RAW		1
#define CS_BASES	(CS_RAW + 1)

/**
 * struct vdso_timestamp - basetime per clock_id
 * @sec:	seconds
 * @nsec:	nanoseconds
 *
 * There is one vdso_timestamp object in vvar for each vDSO-accelerated
 * clock_id. For high-resolution clocks, this encodes the time
 * corresponding to vdso_data.cycle_last. For coarse clocks this encodes
 * the actual time.
 *
 * To be noticed that for highres clocks nsec is left-shifted by
 * vdso_data.cs[x].shift.
 */
struct vdso_timestamp {
	u64	sec;
	u64	nsec;
};

struct timens_offset {
	s64	sec;
	u64	nsec;
};

/**
 * struct vdso_data - vdso datapage representation
 * @seq:		timebase sequence counter
 * @clock_mode:		clock mode
 * @cycle_last:		timebase at clocksource init
 * @mask:		clocksource mask
 * @mult:		clocksource multiplier
 * @shift:		clocksource shift
 * @basetime[clock_id]:	basetime per clock_id
 * @offset[clock_id]:	time namespace offset per clock_id
 * @tz_minuteswest:	minutes west of Greenwich
 * @tz_dsttime:		type of DST correction
 * @hrtimer_res:	hrtimer resolution
 * @__unused:		unused
 * @arch_data:		architecture specific data (optional, defaults
 *			to an empty struct)
 *
 * vdso_data will be accessed by 64 bit and compat code at the same time
 * so we should be careful before modifying this structure.
 *
 * @basetime is used to store the base time for the system wide time getter
 * VVAR page.
 *
 * @offset is used by the special time namespace VVAR pages which are
 * installed instead of the real VVAR page. These namespace pages must set
 * @seq to 1 and @clock_mode to VDSO_CLOCKMODE_TIMENS to force the code into
 * the time namespace slow path. The namespace aware functions retrieve the
 * real system wide VVAR page, read host time and add the per clock offset.
 * For clocks which are not affected by time namespace adjustment the
 * offset must be zero.
 */
struct vdso_data {
	u32			seq;

	s32			clock_mode;
	u64			cycle_last;
	u64			mask;
	u32			mult;
	u32			shift;

	union {
		struct vdso_timestamp	basetime[VDSO_BASES];
		struct timens_offset	offset[VDSO_BASES];
	};

	s32			tz_minuteswest;
	s32			tz_dsttime;
	u32			hrtimer_res;
	u32			__unused;

	struct arch_vdso_data	arch_data;
};

typedef volatile struct vdso_data* 	vdso_data_t;

vdso_data_t get_vdso_data();

void vdso_init(u64 vdso_data_addr, u64 low_res_nsec);

int vdso_clock_gettime(clockid_t clock, struct timespec *ts);
int vdso_gettimeofday(struct timeval *tv, struct timezone *tz);
time_t vdso_time(time_t *time);
int vdso_clock_getres(clockid_t clock, struct timespec *res);
#endif