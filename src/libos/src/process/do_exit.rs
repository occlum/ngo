use std::intrinsics::atomic_store;
use std::sync::Weak;

use super::do_futex::futex_wake;
use super::process::{Process, ProcessFilter};
use super::{table, ProcessRef, StatusChange, TermStatus, ThreadRef, ThreadStatus};
use crate::prelude::*;
use crate::signal::constants::*;
use crate::signal::{KernelSignal, SigNum};
use crate::vm::{CLEAN_REQ_QUEUE, CLEAN_RUNNER, MPMC, USER_SPACE_VM_MANAGER};
use core::ptr;

pub fn do_exit_group(status: i32) {
    let term_status = TermStatus::Exited(status as u8);
    current!().process().force_exit(term_status);
    exit_thread(term_status);
}

pub fn do_exit(status: i32) {
    let term_status = TermStatus::Exited(status as u8);
    exit_thread(term_status);
}

/// Exit this thread if its has been forced to exit.
///
/// A thread may be forced to exit for two reasons: 1) a fatal signal; 2)
/// exit_group syscall.
pub fn handle_force_exit() {
    if current!().process().is_forced_to_exit() {
        exit_thread(current!().process().term_status().unwrap());
    }
}

fn exit_thread(term_status: TermStatus) {
    let thread = current!();
    if thread.status() == ThreadStatus::Exited {
        return;
    }

    let num_remaining_threads = thread.exit(term_status);

    // Notify a thread, if any, that waits on ctid. See set_tid_address(2) for more info.
    if let Some(ctid_ptr) = thread.clear_ctid() {
        unsafe {
            atomic_store(ctid_ptr.as_ptr(), 0);
        }
        futex_wake(ctid_ptr.as_ptr() as *const i32, 1);
    }

    // Keep the main thread's tid available as long as the process is not destroyed.
    // This is important as the user space may still attempt to access the main
    // thread's ThreadRef through the process's pid after the process has become
    // a zombie.
    if thread.tid() != thread.process().pid() {
        table::del_thread(thread.tid()).expect("tid must be in the table");
    }

    // If this thread is the last thread, then exit the process
    if num_remaining_threads == 0 {
        exit_process(&thread, term_status);
    }
}

fn exit_process(thread: &ThreadRef, term_status: TermStatus) {
    let process = thread.process();

    // clean the process vm
    thread.vm().clean_when_exit();

    // Deadlock note: always lock parent first, then child.

    // Lock the idle process since it may adopt new children.
    let idle_ref = super::IDLE.process().clone();
    let mut idle_inner = idle_ref.inner();
    // Lock the parent process as we want to prevent race conditions between
    // current's exit() and parent's wait4().
    let mut parent;
    let mut parent_inner = loop {
        parent = process.parent();
        if parent.pid() == 0 {
            // If the parent is the idle process, don't need to lock again
            break None;
        }

        let parent_inner = parent.inner();
        // To prevent the race condition that parent is changed after `parent()`,
        // but before `parent().innner()`, we need to check again here.
        if parent.pid() != process.parent().pid() {
            continue;
        }
        break Some(parent_inner);
    };
    // The parent is the idle process
    // if parent_inner.is_none() {
    //     // unsafe { *VM_CLEAN_THREAD_RUNNING.get_mut() = false };

    //     // let done = *VM_CLEAN_DONE.lock().unwrap();
    //     // debug_assert!(done == true);
    //     //let ret = unsafe{ libc::pthread_join(VM_CLEAN_THREAD, ptr::null_mut()) };
    // }
    // Lock the current process
    let mut process_inner = process.inner();

    // The parent is the idle process
    if parent_inner.is_none() {
        debug_assert!(parent.pid() == 0);

        let pid = process.pid();
        let main_tid = pid;
        table::del_thread(main_tid).expect("tid must be in the table");
        table::del_process(pid).expect("pid must be in the table");

        process_inner.exit(term_status, &idle_ref, &mut idle_inner, &parent);
        idle_inner.remove_zombie_child(pid);
        wake_host(&process, term_status);
        if !CLEAN_RUNNER.is_empty() {
            let clean_runner = CLEAN_RUNNER.clone();
            while let Ok(req) = CLEAN_RUNNER.try_recv() {
                USER_SPACE_VM_MANAGER.vm_manager().clean_dirty_range(req);
            }
        }
        drop(&*CLEAN_REQ_QUEUE);
        assert!(CLEAN_RUNNER.is_empty());
        USER_SPACE_VM_MANAGER.vm_manager().sort_when_exit();
        // Sadly, this is not true
        // assert!(CLEAN_RUNNER.is_disconnected());
        println!("vm clean thread should exit");
        return;
    }
    // Otherwise, we need to notify the parent process
    let mut parent_inner = parent_inner.unwrap();
    process_inner.exit(term_status, &idle_ref, &mut idle_inner, &parent);

    //Send SIGCHLD to parent
    send_sigchld_to(&parent);

    drop(idle_inner);
    drop(parent_inner);
    drop(process_inner);

    // Notify the parent that this child process's status has changed
    process
        .notifier()
        .broadcast(&StatusChange::Terminated(term_status));

    // Notify the host threads that wait the status change of this process
    wake_host(&process, term_status);
}

fn send_sigchld_to(parent: &Arc<Process>) {
    let signal = Box::new(KernelSignal::new(SigNum::from(SIGCHLD)));
    let mut sig_queues = parent.sig_queues().write().unwrap();
    sig_queues.enqueue(signal);
}

fn wake_host(process: &ProcessRef, term_status: TermStatus) {
    if let Some(host_waker) = process.host_waker() {
        host_waker.wake(term_status);
    }
}
