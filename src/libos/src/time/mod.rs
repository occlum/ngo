use self::timer_slack::*;
use super::*;
use async_rt::wait::Waiter;
use core::convert::TryFrom;
use process::pid_t;
use rcore_fs::dev::TimeProvider;
use rcore_fs::vfs::Timespec;
use std::time::Duration;
use std::{fmt, u64};
pub use vdso_time::ClockId;

pub mod timer_slack;
pub mod up_time;

pub use timer_slack::TIMERSLACK;

#[allow(non_camel_case_types)]
pub type time_t = i64;

#[allow(non_camel_case_types)]
pub type suseconds_t = i64;

#[allow(non_camel_case_types)]
pub type clock_t = i64;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
#[allow(non_camel_case_types)]
pub struct timeval_t {
    sec: time_t,
    usec: suseconds_t,
}

impl timeval_t {
    pub fn new(sec: time_t, usec: suseconds_t) -> Self {
        let time = Self { sec, usec };

        time.validate().unwrap();
        time
    }

    pub fn validate(&self) -> Result<()> {
        if self.sec >= 0 && self.usec >= 0 && self.usec < 1_000_000 {
            Ok(())
        } else {
            return_errno!(EINVAL, "invalid value for timeval_t");
        }
    }

    pub fn as_duration(&self) -> Duration {
        Duration::new(self.sec as u64, (self.usec * 1_000) as u32)
    }
}

impl From<Duration> for timeval_t {
    fn from(duration: Duration) -> timeval_t {
        let sec = duration.as_secs() as time_t;
        let usec = duration.subsec_micros() as i64;
        debug_assert!(sec >= 0); // nsec >= 0 always holds
        timeval_t { sec, usec }
    }
}

pub fn do_gettimeofday() -> timeval_t {
    let tv = timeval_t::from(vdso_time::clock_gettime(ClockId::CLOCK_REALTIME).unwrap());
    tv.validate()
        .expect("gettimeofday returned invalid timeval_t");
    tv
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
#[allow(non_camel_case_types)]
pub struct timespec_t {
    sec: time_t,
    nsec: i64,
}

impl From<Duration> for timespec_t {
    fn from(duration: Duration) -> timespec_t {
        let sec = duration.as_secs() as time_t;
        let nsec = duration.subsec_nanos() as i64;
        debug_assert!(sec >= 0); // nsec >= 0 always holds
        timespec_t { sec, nsec }
    }
}

impl timespec_t {
    pub fn from_raw_ptr(ptr: *const timespec_t) -> Result<timespec_t> {
        let ts = unsafe { *ptr };
        ts.validate()?;
        Ok(ts)
    }

    pub fn validate(&self) -> Result<()> {
        if self.sec >= 0 && self.nsec >= 0 && self.nsec < 1_000_000_000 {
            Ok(())
        } else {
            return_errno!(EINVAL, "invalid value for timespec_t");
        }
    }

    pub fn sec(&self) -> time_t {
        self.sec
    }

    pub fn nsec(&self) -> i64 {
        self.nsec
    }

    pub fn as_duration(&self) -> Duration {
        Duration::new(self.sec as u64, self.nsec as u32)
    }
}

#[allow(non_camel_case_types)]
pub type clockid_t = i32;

pub fn do_clock_gettime(clockid: ClockId) -> Result<timespec_t> {
    // TODO: support CLOCK_PROCESS_CPUTIME_ID and CLOCK_THREAD_CPUTIME_ID.
    if clockid == ClockId::CLOCK_PROCESS_CPUTIME_ID || clockid == ClockId::CLOCK_THREAD_CPUTIME_ID {
        return_errno!(
            EINVAL,
            "Not support CLOCK_PROCESS_CPUTIME_ID or CLOCK_THREAD_CPUTIME_ID"
        );
    }
    let tv = timespec_t::from(vdso_time::clock_gettime(clockid).unwrap());
    tv.validate()
        .expect("clock_gettime returned invalid timespec");
    Ok(tv)
}

pub fn do_clock_getres(clockid: ClockId) -> Result<timespec_t> {
    let res = timespec_t::from(vdso_time::clock_getres(clockid).unwrap());
    let validate_resolution = |res: &timespec_t| -> Result<()> {
        // The resolution can be ranged from 1 nanosecond to a few milliseconds
        if res.sec == 0 && res.nsec > 0 && res.nsec < 1_000_000_000 {
            Ok(())
        } else {
            return_errno!(EINVAL, "invalid value for resolution");
        }
    };
    // do sanity check
    validate_resolution(&res).expect("clock_getres returned invalid resolution");
    Ok(res)
}

pub async fn do_nanosleep(req: &timespec_t, rem: Option<&mut timespec_t>) -> Result<()> {
    let waiter = Waiter::new();
    let mut duration = Duration::new(req.sec as u64, req.nsec as u32);
    if let Ok(_) = waiter.wait_timeout(Some(&mut duration)).await {
        // TODO: support interrupt sleep.
        // return_errno!(EINTR, "sleep interrupted");
        unreachable!("this waiter can not be interrupted");
    }

    if let Some(rem) = rem {
        // wait_timeout() can guarantee that rem <= req.
        *rem = timespec_t {
            sec: duration.as_secs() as i64,
            nsec: duration.subsec_nanos() as i64,
        };
    }
    Ok(())
}

pub fn do_thread_getcpuclock() -> Result<timespec_t> {
    extern "C" {
        fn occlum_ocall_thread_getcpuclock(ret: *mut c_int, tp: *mut timespec_t) -> sgx_status_t;
    }

    let mut tv: timespec_t = Default::default();
    try_libc!({
        let mut retval: i32 = 0;
        let status = occlum_ocall_thread_getcpuclock(&mut retval, &mut tv as *mut timespec_t);
        assert!(status == sgx_status_t::SGX_SUCCESS);
        retval
    });
    tv.validate()?;
    Ok(tv)
}

pub fn do_rdtsc() -> (u32, u32) {
    extern "C" {
        fn occlum_ocall_rdtsc(low: *mut u32, high: *mut u32) -> sgx_status_t;
    }
    let mut low = 0;
    let mut high = 0;
    let sgx_status = unsafe { occlum_ocall_rdtsc(&mut low, &mut high) };
    assert!(sgx_status == sgx_status_t::SGX_SUCCESS);
    (low, high)
}

// For SEFS
pub struct OcclumTimeProvider;

impl TimeProvider for OcclumTimeProvider {
    fn current_time(&self) -> Timespec {
        let time = do_gettimeofday();
        Timespec {
            sec: time.sec,
            nsec: time.usec as i32 * 1000,
        }
    }
}
