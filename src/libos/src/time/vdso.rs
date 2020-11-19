use super::*;
use std::sync::{atomic, Arc, SgxMutex};

lazy_static! {
    pub(crate) static ref VDSO: Vdso = Vdso::new();
}

pub struct Vdso {
    vdso_data_addr: u64,
    low_res_nsec: u64,
}

impl Vdso {
    pub fn new() -> Vdso {
        extern "C" {
            fn occlum_ocall_get_vdso_info(
                vdso_data_addr: *mut u64,
                low_res_nsec: *mut u64,
            ) -> sgx_status_t;
        }

        let mut vdso_data_addr: u64 = 0;
        let mut low_res_nsec: u64 = 0;
        unsafe {
            occlum_ocall_get_vdso_info(&mut vdso_data_addr, &mut low_res_nsec);
        }

        debug!(
            "vdso_data_addr: {:?}, low_res_nsec: {}",
            vdso_data_addr, low_res_nsec
        );

        extern "C" {
            fn vdso_init(vdso_data_addr: u64, low_res_nsec: u64);
        }
        unsafe {
            vdso_init(vdso_data_addr, low_res_nsec);
        }

        Self {
            vdso_data_addr,
            low_res_nsec,
        }
    }

    pub fn gettimeofday(&self, tv: *mut timeval_t, tz: *mut timezone_t) -> i32 {
        extern "C" {
            fn vdso_gettimeofday(tv: *mut timeval_t, tz: *mut timezone_t) -> i32;
        }
        unsafe { vdso_gettimeofday(tv, tz) }
    }

    pub fn clock_gettime(&self, clockid: clockid_t, tp: *mut timespec_t) -> i32 {
        extern "C" {
            fn vdso_clock_gettime(clockid: clockid_t, tp: *mut timespec_t) -> i32;
        }
        unsafe { vdso_clock_gettime(clockid, tp) }
    }

    pub fn clock_getres(&self, clockid: clockid_t, res: *mut timespec_t) -> i32 {
        extern "C" {
            fn vdso_clock_getres(clockid: clockid_t, res: *mut timespec_t) -> i32;
        }
        unsafe { vdso_clock_getres(clockid, res) }
    }
}
