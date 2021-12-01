use super::c_types::{mcontext_t, siginfo_t, ucontext_t};
use super::constants::SIGKILL;
use super::sig_stack::SigStackFlags;
use super::{SigAction, SigActionFlags, SigDefaultAction, SigSet, Signal};
use crate::entry::context_switch::{CpuContext, FpRegs, CURRENT_CONTEXT};
use crate::lazy_static::__Deref;
use crate::prelude::*;
use crate::process::{ProcessRef, TermStatus, ThreadRef};
use aligned::{Aligned, A16};
use core::arch::x86_64::{_fxrstor, _fxsave};
use std::{ptr, slice};

pub fn do_rt_sigreturn() -> Result<()> {
    debug!("do_rt_sigreturn");
    let last_ucontext = {
        let last_ucontext = PRE_UCONTEXTS.with(|ref_cell| {
            let mut stack = ref_cell.borrow_mut();
            stack.pop()
        });

        // Handle a (very unlikely) error condition
        if last_ucontext.is_none() {
            let term_status = TermStatus::Killed(SIGKILL);
            current!().process().force_exit(term_status);
            return_errno!(
                EINVAL,
                "sigreturn should not have been called; kill this process"
            );
        }

        unsafe { &*last_ucontext.unwrap() }
    };

    // Restore sigmask
    current!().set_sig_mask(SigSet::from_c(last_ucontext.uc_sigmask));
    // Restore user context
    CURRENT_CONTEXT.with(|_context| {
        let mut context = _context.borrow_mut();
        context.gp_regs = last_ucontext.uc_mcontext.gp_regs;
        unsafe {
            context.fp_regs.save_from_slice(&last_ucontext.fpregs);
        }
    });
    Ok(())
}

/// Deliver a queued signal for the current thread, respecting the thread's
/// signal mask.
///
/// The delivery of a signal means two things: 1) dequeuing the signal from
/// the per-thread or per-process signal queue, and 2) handling the signal
/// according to the signal disposition.
///
/// When handling a signal, one of the three actions below will be done:
///
/// 1. Ignore the signal. This is the easy part.
///
/// 2. Terminate the process if the signal is fatal. This is called "force exit".
///
/// 3. Call a user-registered signal handler. In this case, the current CPU context
/// will be modified so that the user-registered signal handler will be called
/// upon returning to the user space when the current syscall is finished.
///
/// **Requirement.** This must be called only once during the execution of a
/// syscall and at a very late stage.
///
/// **Post-condition.** The temporary signal mask of the current thread is cleared.
///
/// **Interaction with force_signal.** If force_signal is called during a syscall,
/// then deliver_signal won't deliver any signals.
pub fn deliver_signal() {
    let thread = current!();
    let process = thread.process();

    if process.is_forced_to_exit() {
        return;
    }

    if !forced_signal_flag::get() {
        do_deliver_signal(&thread, &process);
    } else {
        forced_signal_flag::reset();
    }
}

fn do_deliver_signal(thread: &ThreadRef, process: &ProcessRef) {
    loop {
        if process.sig_queues().read().unwrap().empty()
            && thread.sig_queues().read().unwrap().empty()
        {
            return;
        }

        // Dequeue a signal, respecting the signal mask
        let signal = {
            let sig_mask = thread.sig_mask();
            let signal_opt = process
                .sig_queues()
                .write()
                .unwrap()
                .dequeue(&sig_mask)
                .or_else(|| thread.sig_queues().write().unwrap().dequeue(&sig_mask));
            if signal_opt.is_none() {
                return;
            }
            signal_opt.unwrap()
        };

        let continue_handling = handle_signal(signal, thread, process);
        if !continue_handling {
            break;
        }
    }
}

/// Force delivering the given signal to the current thread, without checking the thread's
/// signal mask.
///
/// **Post-condition.** The tmp signal mask of the current thread is all set. This avoids
/// delivering two signals during one execution of a syscall.
///
/// **Requirement.** This function can only be called at most once during the execution of
/// a syscall.
pub fn force_signal(signal: Box<dyn Signal>) {
    let thread = current!();
    let process = thread.process();

    assert!(forced_signal_flag::get() == false);
    forced_signal_flag::set();

    handle_signal(signal, &thread, &process);
}

fn handle_signal(signal: Box<dyn Signal>, thread: &ThreadRef, process: &ProcessRef) -> bool {
    let is_sig_stack_full = PRE_UCONTEXTS.with(|ref_cell| {
        let stack = ref_cell.borrow();
        stack.full()
    });
    if is_sig_stack_full {
        panic!("the nested signal is too deep to handle");
    }

    let action = process.sig_dispositions().read().unwrap().get(signal.num());
    debug!(
        "Handle signal: signal: {:?}, action: {:?}",
        &signal, &action
    );

    let continue_handling = match action {
        SigAction::Ign => true,
        SigAction::Dfl => {
            let default_action = SigDefaultAction::from_signum(signal.num());
            match default_action {
                SigDefaultAction::Ign => true,
                SigDefaultAction::Term | SigDefaultAction::Core => {
                    let term_status = TermStatus::Killed(signal.num());
                    process.force_exit(term_status);
                    false
                }
                SigDefaultAction::Stop => {
                    warn!("SIGSTOP is unsupported");
                    true
                }
                SigDefaultAction::Cont => {
                    warn!("SIGCONT is unsupported");
                    true
                }
            }
        }
        SigAction::User {
            handler_addr,
            flags,
            restorer_addr,
            mask,
        } => {
            let ret = CURRENT_CONTEXT.with(|_context| {
                let mut context = _context.borrow_mut();
                handle_signals_by_user(
                    signal,
                    thread,
                    handler_addr,
                    flags,
                    restorer_addr,
                    mask,
                    context.deref_mut(),
                )
            });
            if let Err(_) = ret {
                todo!("kill the process if any error");
            }
            false
        }
    };
    continue_handling
}

fn handle_signals_by_user(
    signal: Box<dyn Signal>,
    thread: &ThreadRef,
    handler_addr: usize,
    flags: SigActionFlags,
    restorer_addr: usize,
    new_sig_mask: SigSet,
    curr_user_ctxt: &mut CpuContext,
) -> Result<()> {
    // Set a new signal mask and get the old one
    let new_sig_mask = if flags.contains(SigActionFlags::SA_NODEFER) {
        new_sig_mask
    } else {
        // Block the current signal while executing the signal handler
        new_sig_mask + signal.num()
    };
    let old_sig_mask = thread.set_sig_mask(new_sig_mask);

    // Represent the user stack in a memory safe way
    let mut user_stack = {
        let get_stack_top = || -> usize {
            if flags.contains(SigActionFlags::SA_ONSTACK) {
                let thread = current!();
                let sig_stack = thread.sig_stack().lock().unwrap();
                if let Some(stack) = *sig_stack {
                    if !stack.contains(curr_user_ctxt.gp_regs.rsp as usize) {
                        let stack_top = stack.sp() + stack.size();
                        return stack_top;
                    }
                }
            }
            const BIG_ENOUGH_GAP: u64 = 1024;
            let stack_top = (curr_user_ctxt.gp_regs.rsp - BIG_ENOUGH_GAP) as usize;
            stack_top
        };
        let stack_top = get_stack_top();
        let stack_size = {
            const BIG_ENOUGH_SIZE: u64 = 4096;
            BIG_ENOUGH_SIZE as usize
        };
        // TODO: validate the memory range of the stack
        unsafe { Stack::new(stack_top, stack_size)? }
    };

    // Prepare the user stack in four steps.
    //
    // 1. Allocate and init siginfo_t on the user stack.
    let info = {
        let info = user_stack.alloc::<siginfo_t>()?;
        *info = signal.to_info();
        info as *mut siginfo_t
    };
    // 2. Allocate and init ucontext_t on the user stack.
    let ucontext = {
        // The x86 calling convention requires rsp to be 16-byte aligned.
        // The following allocation on stack is right before we "call" the
        // signal handler. So we need to make sure the allocation is at least
        // 16-byte aligned.
        let ucontext = user_stack.alloc_aligned::<ucontext_t>(16)?;

        // TODO: set all fields in ucontext
        *ucontext = unsafe { std::mem::zeroed() };
        // Save the old sigmask
        ucontext.uc_sigmask = old_sig_mask.to_c();
        // Save the user context
        ucontext.uc_mcontext.gp_regs = curr_user_ctxt.gp_regs;
        // Save the floating point registers
        let fp_regs = &mut curr_user_ctxt.fp_regs;
        if !fp_regs.is_valid() {
            // We need a valid fxsave structure in the buffer,
            // because the app may modify part of it to update the
            // floating point after the signal handler finished.
            fp_regs.save();
        }
        ucontext.fpregs.copy_from_slice(fp_regs.as_slice());

        ucontext as *mut ucontext_t
    };
    // 3. Set up the call return address on the stack before we "call" the signal handler
    let handler_stack_top = {
        let handler_stack_top = user_stack.alloc::<usize>()?;
        *handler_stack_top = restorer_addr;
        handler_stack_top as *mut usize
    };

    // Modify the current user CPU context so that the signal handler will
    // be "called" upon returning back to the user space and when the signal
    // handler finishes, the CPU will jump to the restorer.
    {
        let gp_regs = &mut curr_user_ctxt.gp_regs;
        gp_regs.rsp = handler_stack_top as u64;
        gp_regs.rip = handler_addr as u64;
        // Prepare the three arguments for the signal handler
        gp_regs.rdi = signal.num().as_u8() as u64;
        gp_regs.rsi = info as u64;
        gp_regs.rdx = ucontext as u64;

        let fp_regs = &mut curr_user_ctxt.fp_regs;
        fp_regs.clear();
    }

    PRE_UCONTEXTS.with(|ref_cell| {
        let mut stack = ref_cell.borrow_mut();
        stack.push(ucontext).unwrap();
    });
    Ok(())
}

/// Represent and manipulate a stack in a memory-safe way
struct Stack {
    pointer: usize,
    bottom: usize,
}

impl Stack {
    /// Create a new region of memory to use as stack
    pub unsafe fn new(stack_top: usize, stack_size: usize) -> Result<Stack> {
        if stack_top <= stack_size {
            return_errno!(EINVAL, "stack address may underflow");
        }
        let pointer = stack_top;
        let bottom = stack_top - stack_size;
        Ok(Stack { pointer, bottom })
    }

    /// Get the size of the free space in the stack
    pub fn size(&self) -> usize {
        self.pointer - self.bottom
    }

    /// Allocate a mutable object on the stack.
    ///
    /// The alignment of the object will be `std::mem::size_of::<T>()`.
    pub fn alloc<T>(&mut self) -> Result<&mut T> {
        self.do_alloc_aligned::<T>(1)
    }

    /// Allocate a mutable object on the stack.
    ///
    /// The alignment of the object will be `max(align, std::mem::size_of::<T>())`.
    pub fn alloc_aligned<T>(&mut self, align: usize) -> Result<&mut T> {
        if !align.is_power_of_two() {
            return_errno!(EINVAL, "align must be a power of two");
        }
        self.do_alloc_aligned::<T>(align)
    }

    /// Allocate a mutable object on the stack.
    ///
    /// The alignment of the object will be `max(align, std::mem::size_of::<T>())`.
    fn do_alloc_aligned<T>(&mut self, align: usize) -> Result<&mut T> {
        // Check precondition
        debug_assert!(align.is_power_of_two());

        // Calculate the pointer of the object
        let new_pointer = {
            let size = std::mem::size_of::<T>();
            let align = std::mem::align_of::<T>().max(align);

            let mut pointer = self.pointer;
            if pointer < size {
                return_errno!(ENOMEM, "not enough memory");
            }
            pointer -= size;
            pointer = align_down(pointer, align);
            if pointer < self.bottom {
                return_errno!(ENOMEM, "not enough memory");
            }
            pointer
        };
        self.pointer = new_pointer;

        let obj_ref = unsafe { &mut *(new_pointer as *mut T) };
        Ok(obj_ref)
    }
}

thread_local! {
    static PRE_UCONTEXTS: RefCell<CpuContextStack> = Default::default();
}

#[derive(Debug, Default)]
struct CpuContextStack {
    stack: [Option<*mut ucontext_t>; 32],
    count: usize,
}

impl CpuContextStack {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn full(&self) -> bool {
        self.count == self.stack.len()
    }

    pub fn empty(&self) -> bool {
        self.count == 0
    }

    pub fn push(&mut self, cpu_context: *mut ucontext_t) -> Result<()> {
        if self.full() {
            return_errno!(ENOMEM, "cpu context stack is full");
        }
        self.stack[self.count] = Some(cpu_context);
        self.count += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<*mut ucontext_t> {
        if self.empty() {
            return None;
        }
        self.count -= 1;
        self.stack[self.count].take()
    }
}

// This module maintain a flag about whether a task already has a forced signal.
// The goal is to ensure that during the execution of a syscall at most one
// signal is forced.
mod forced_signal_flag {
    use core::cell::Cell;

    pub fn get() -> bool {
        HAS_FORCED_SIGNAL.with(|has_forced_signal| has_forced_signal.get())
    }

    pub fn set() {
        HAS_FORCED_SIGNAL.with(|has_forced_signal| {
            has_forced_signal.set(true);
        })
    }

    pub fn reset() {
        HAS_FORCED_SIGNAL.with(|has_forced_signal| {
            has_forced_signal.set(false);
        })
    }

    task_local! {
        static HAS_FORCED_SIGNAL: Cell<bool> = Cell::new(false);
    }
}
