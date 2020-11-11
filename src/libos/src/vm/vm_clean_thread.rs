use super::*;
use crate::libc::{pthread_attr_t, pthread_t};
use crate::process::table::{get_all_processes, get_all_threads};
use core::mem;
use core::ptr;

pub static mut VM_CLEAN_THREAD: libc::pthread_t = 0 as libc::pthread_t;
pub static mut VM_CLEAN_THREAD_RUNNING: bool = false;

lazy_static! {
// Clean all munmapped ranges before exit
pub static ref VM_CLEAN_DONE: SgxMutex<bool> = SgxMutex::new(false);
}

pub fn init_vm_clean_thread() -> Result<()> {
    unsafe {
        let mut arg: libc::c_void = mem::zeroed();
        let attr: libc::pthread_attr_t = mem::zeroed();
        VM_CLEAN_THREAD_RUNNING = true;
        let ret = libc::pthread_create(
            &mut VM_CLEAN_THREAD as *mut pthread_t,
            &attr as *const pthread_attr_t,
            mem_worker_thread_start,
            &mut arg as *mut c_void,
        );
        //println!("init native = {:?}", VM_CLEAN_THREAD as libc::pthread_t);
    }
    Ok(())
}

pub extern "C" fn mem_worker_thread_start(main: *mut libc::c_void) -> *mut libc::c_void {
    let mut done = VM_CLEAN_DONE.lock().unwrap();
    while unsafe { VM_CLEAN_THREAD_RUNNING } {
        //println!("in a custom thread");
        let all_process = get_all_processes();
        for process in all_process.iter() {
            if let Some(thread) = process.main_thread() {
                thread
                    .vm()
                    .get_mmap_manager()
                    .clean_dirty_range_in_bgthread();
            }
        }
    }
    *done = true;
    drop(done);

    ptr::null_mut()
}

extern "C" {
    fn pthread_create(
        native: *mut pthread_t,
        attr: *const pthread_attr_t,
        f: extern "C" fn(*mut c_void) -> *mut c_void,
        value: *mut c_void,
    ) -> c_int;
}
