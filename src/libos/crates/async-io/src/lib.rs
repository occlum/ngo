#![cfg_attr(feature = "sgx", no_std)]
#![feature(coerce_unsized)]
#![feature(unsize)]

#[cfg(feature = "sgx")]
extern crate sgx_types;
#[cfg(feature = "sgx")]
#[macro_use]
extern crate sgx_tstd as std;
#[cfg(feature = "sgx")]
extern crate sgx_libc as libc;

#[macro_use]
extern crate log;

pub mod event;
pub mod file;
pub mod fs;
pub mod ioctl;
pub mod prelude;
pub mod socket;
pub mod util;
