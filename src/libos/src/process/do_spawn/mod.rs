use std::convert::TryFrom;
use std::ffi::{CStr, CString};
use std::path::Path;

use self::aux_vec::{AuxKey, AuxVec};
use self::exec_loader::{load_exec_file_hdr_to_vec, load_file_hdr_to_vec};
use super::elf_file::{ElfFile, ElfHeader, ProgramHeaderExt};
use super::process::ProcessBuilder;
use super::spawn_attribute::SpawnAttr;
use super::thread::ThreadName;
use super::{table, HostWaker, ProcessRef, ThreadRef};
use crate::entry::context_switch::{CpuContext, GpRegs};
use crate::fs::{
    CreationFlags, FileDesc, FileMode, FileTable, FsPath, FsView, HostStdioFds, StdinFile,
    StdoutFile,
};
use crate::prelude::*;
use crate::vm::ProcessVM;

mod aux_vec;
mod exec_loader;
mod init_stack;
mod init_vm;

/// Spawn a new process and execute it in a new host thread.
pub fn do_spawn(
    elf_path: &str,
    argv: &[CString],
    envp: &[CString],
    file_actions: &[FileAction],
    spawn_attributes: Option<SpawnAttr>,
    current_ref: &ThreadRef,
) -> Result<pid_t> {
    let exec_now = true;
    do_spawn_common(
        elf_path,
        argv,
        envp,
        file_actions,
        spawn_attributes,
        None,
        None,
        current_ref,
        exec_now,
    )
}

/// Spawn a new process but execute it later.
pub fn do_spawn_root(
    elf_path: &str,
    argv: &[CString],
    envp: &[CString],
    file_actions: &[FileAction],
    spawn_attributes: Option<SpawnAttr>,
    host_stdio_fds: &HostStdioFds,
    wake_host: *mut i32,
    current_ref: &ThreadRef,
) -> Result<pid_t> {
    let exec_now = false;
    do_spawn_common(
        elf_path,
        argv,
        envp,
        file_actions,
        spawn_attributes,
        Some(host_stdio_fds),
        Some(wake_host),
        current_ref,
        exec_now,
    )
}

fn do_spawn_common(
    elf_path: &str,
    argv: &[CString],
    envp: &[CString],
    file_actions: &[FileAction],
    spawn_attributes: Option<SpawnAttr>,
    host_stdio_fds: Option<&HostStdioFds>,
    wake_host: Option<*mut i32>,
    current_ref: &ThreadRef,
    exec_now: bool,
) -> Result<pid_t> {
    let (new_process_ref, init_cpu_state) = new_process(
        elf_path,
        argv,
        envp,
        file_actions,
        spawn_attributes,
        host_stdio_fds,
        wake_host,
        current_ref,
    )?;

    let new_main_thread = new_process_ref
        .main_thread()
        .expect("the main thread is just created; it must exist");
    async_rt::task::spawn(crate::entry::thread::main_loop(
        new_main_thread,
        init_cpu_state,
    ));

    let new_pid = new_process_ref.pid();
    Ok(new_pid)
}

/// Create a new process and its main thread.
fn new_process(
    file_path: &str,
    argv: &[CString],
    envp: &[CString],
    file_actions: &[FileAction],
    spawn_attributes: Option<SpawnAttr>,
    host_stdio_fds: Option<&HostStdioFds>,
    wake_host_ptr: Option<*mut i32>,
    current_ref: &ThreadRef,
) -> Result<(ProcessRef, CpuContext)> {
    let mut argv = argv.clone().to_vec();
    let (is_script, elf_inode, mut elf_buf, elf_header) =
        load_exec_file_hdr_to_vec(file_path, current_ref)?;

    // elf_path might be different from file_path because file_path could lead to a script text file.
    // And intepreter will be the loaded ELF.
    let elf_path = if let Some(interpreter_path) = is_script {
        if argv.len() == 0 {
            return_errno!(EINVAL, "argv[0] not found");
        }
        argv.insert(0, CString::new(interpreter_path.as_str())?);
        argv[1] = CString::new(file_path)?; // script file needs to be the full path
        interpreter_path
    } else {
        file_path.to_string()
    };

    let exec_elf_hdr = ElfFile::new(&elf_inode, &mut elf_buf, elf_header)
        .cause_err(|e| errno!(e.errno(), "invalid executable"))?;
    let ldso_path = exec_elf_hdr
        .elf_interpreter()
        .ok_or_else(|| errno!(EINVAL, "cannot find the interpreter segment"))?;
    trace!("ldso_path = {:?}", ldso_path);
    let (ldso_inode, mut ldso_elf_hdr_buf, ldso_elf_header) =
        load_file_hdr_to_vec(ldso_path, current_ref)
            .cause_err(|e| errno!(e.errno(), "cannot load ld.so"))?;
    let ldso_elf_header = if ldso_elf_header.is_none() {
        return_errno!(ENOEXEC, "ldso header is not ELF format");
    } else {
        ldso_elf_header.unwrap()
    };
    let ldso_elf_hdr = ElfFile::new(&ldso_inode, &mut ldso_elf_hdr_buf, ldso_elf_header)
        .cause_err(|e| errno!(e.errno(), "invalid ld.so"))?;

    let (new_process_ref, init_cpu_state) = {
        let process_ref = current_ref.process().clone();

        let vm = init_vm::do_init(&exec_elf_hdr, &ldso_elf_hdr)?;
        let mut auxvec = init_auxvec(&vm, &exec_elf_hdr)?;

        // Notify debugger to load the symbols from elf file
        let ldso_elf_base = vm.get_elf_ranges()[1].start() as u64;
        unsafe {
            occlum_gdb_hook_load_elf(
                ldso_elf_base,
                ldso_path.as_ptr() as *const u8,
                ldso_path.len() as u64,
            );
        }
        let exec_elf_base = vm.get_elf_ranges()[0].start() as u64;
        unsafe {
            occlum_gdb_hook_load_elf(
                exec_elf_base,
                elf_path.as_ptr() as *const u8,
                elf_path.len() as u64,
            );
        }

        let init_cpu_state = {
            let ldso_entry = {
                let ldso_range = vm.get_elf_ranges()[1];
                let ldso_entry = ldso_range.start() + ldso_elf_hdr.elf_header().e_entry as usize;
                if !ldso_range.contains(ldso_entry) {
                    return_errno!(EINVAL, "Invalid program entry");
                }
                ldso_entry
            };
            let user_stack_base = vm.get_stack_base();
            let user_stack_limit = vm.get_stack_limit();
            let user_rsp = init_stack::do_init(user_stack_base, 4096, &argv, envp, &mut auxvec)?;

            // Set the default user fsbase to an address on user stack, which is
            // a relatively safe address in case the user program uses %fs before
            // initializing fs base address.
            CpuContext {
                gp_regs: GpRegs {
                    rsp: user_rsp as _,
                    rip: ldso_entry as _,
                    ..Default::default()
                },
                fs_base: user_stack_limit as _,
                ..Default::default()
            }
        };
        let vm_ref = Arc::new(vm);
        let files_ref = {
            let files = init_files(current_ref, file_actions, host_stdio_fds)?;
            Arc::new(SgxMutex::new(files))
        };
        let fs_ref = Arc::new(RwLock::new(current_ref.fs().read().unwrap().clone()));
        let sched_ref = Arc::new(SgxMutex::new(current_ref.sched().lock().unwrap().clone()));
        let rlimit_ref = Arc::new(SgxMutex::new(current_ref.rlimits().lock().unwrap().clone()));
        let sig_mask = if spawn_attributes.is_some() && spawn_attributes.unwrap().sig_mask.is_some()
        {
            spawn_attributes.unwrap().sig_mask.unwrap()
        } else {
            current_ref.sig_mask()
        };
        trace!("new process sigmask = {:?}", sig_mask);

        let mut sig_dispositions = current_ref
            .process()
            .sig_dispositions()
            .read()
            .unwrap()
            .clone();
        sig_dispositions.inherit();
        if spawn_attributes.is_some() && spawn_attributes.unwrap().sig_default.is_some() {
            let sig_default_set = spawn_attributes.unwrap().sig_default.unwrap();
            sig_default_set.iter().for_each(|b| {
                sig_dispositions.set_default(b);
            })
        }
        trace!("new process sig_dispositions = {:?}", sig_dispositions);

        // Make the default thread name to be the process's corresponding elf file name
        let elf_name = elf_path.rsplit('/').collect::<Vec<&str>>()[0];
        let thread_name = ThreadName::new(elf_name);

        let mut builder = ProcessBuilder::new()
            .vm(vm_ref)
            .exec_path(&elf_path)
            .umask(process_ref.umask())
            .parent(process_ref)
            .sched(sched_ref)
            .rlimits(rlimit_ref)
            .fs(fs_ref)
            .files(files_ref)
            .name(thread_name)
            .sig_mask(sig_mask)
            .sig_dispositions(sig_dispositions);

        if let Some(wake_host_ptr) = wake_host_ptr {
            let host_waker = HostWaker::new(wake_host_ptr)?;
            builder = builder.host_waker(host_waker);
        }

        let new_process = builder.build()?;
        (new_process, init_cpu_state)
    };

    table::add_process(new_process_ref.clone());
    table::add_thread(new_process_ref.main_thread().unwrap());

    info!(
        "Process created: elf = {}, pid = {}",
        elf_path,
        new_process_ref.pid()
    );

    Ok((new_process_ref, init_cpu_state))
}

#[derive(Debug)]
pub enum FileAction {
    /// open(path, oflag, mode) had been called, and the returned file
    /// descriptor, if not `fd`, had been changed to `fd`.
    Open {
        path: String,
        mode: u32,
        oflag: u32,
        fd: FileDesc,
    },
    Dup2(FileDesc, FileDesc),
    Close(FileDesc),
}

fn init_files(
    current_ref: &ThreadRef,
    file_actions: &[FileAction],
    host_stdio_fds: Option<&HostStdioFds>,
) -> Result<FileTable> {
    // Usually, we just inherit the file table from the current process
    let should_inherit_file_table = current_ref.process().pid() > 0;
    if should_inherit_file_table {
        // Fork: clone file table
        let mut cloned_file_table = current_ref.files().lock().unwrap().clone();
        // Perform file actions to modify the cloned file table
        for file_action in file_actions {
            match file_action {
                &FileAction::Open {
                    ref path,
                    mode,
                    oflag,
                    fd,
                } => {
                    let inode_file = current_ref.fs().read().unwrap().open_file(
                        &FsPath::try_from(path.as_str())?,
                        oflag,
                        FileMode::from_bits_truncate(mode as u16),
                    )?;
                    let file_ref = FileRef::new_inode(inode_file);
                    let creation_flags = CreationFlags::from_bits_truncate(oflag);
                    cloned_file_table.put_at(fd, file_ref, creation_flags.must_close_on_spawn());
                }
                &FileAction::Dup2(old_fd, new_fd) => {
                    let file = cloned_file_table.get(old_fd)?;
                    if old_fd != new_fd {
                        cloned_file_table.put_at(new_fd, file, false);
                    }
                }
                &FileAction::Close(fd) => {
                    // ignore error
                    cloned_file_table.del(fd);
                }
            }
        }
        // Exec: close fd with close_on_spawn
        cloned_file_table.close_on_spawn();
        return Ok(cloned_file_table);
    }

    // But, for init process, we initialize file table for it
    let mut file_table = FileTable::new();
    let stdin = FileRef::new_file(StdinFile::new(host_stdio_fds.unwrap().stdin_fd as FileDesc));
    let stdout = FileRef::new_file(StdoutFile::new(
        host_stdio_fds.unwrap().stdout_fd as FileDesc,
    ));
    let stderr = FileRef::new_file(StdoutFile::new(
        host_stdio_fds.unwrap().stderr_fd as FileDesc,
    ));

    file_table.put(stdin, false);
    file_table.put(stdout, false);
    file_table.put(stderr, false);
    Ok(file_table)
}

fn init_auxvec(process_vm: &ProcessVM, exec_elf_file: &ElfFile) -> Result<AuxVec> {
    let mut auxvec = AuxVec::new();
    auxvec.set(AuxKey::AT_PAGESZ, 4096)?;
    auxvec.set(AuxKey::AT_UID, 0)?;
    auxvec.set(AuxKey::AT_GID, 0)?;
    auxvec.set(AuxKey::AT_EUID, 0)?;
    auxvec.set(AuxKey::AT_EGID, 0)?;
    auxvec.set(AuxKey::AT_SECURE, 0)?;
    auxvec.set(AuxKey::AT_SYSINFO, 0)?;

    let exec_elf_base = process_vm.get_elf_ranges()[0].start() as u64;
    let exec_elf_header = exec_elf_file.elf_header();
    auxvec.set(AuxKey::AT_PHENT, exec_elf_header.e_phentsize as u64)?;
    auxvec.set(AuxKey::AT_PHNUM, exec_elf_header.e_phnum as u64)?;
    auxvec.set(AuxKey::AT_PHDR, exec_elf_base + exec_elf_header.e_phoff)?;

    let base_load_address_offset = exec_elf_file.base_load_address_offset();
    auxvec.set(
        AuxKey::AT_ENTRY,
        exec_elf_base + exec_elf_header.e_entry - base_load_address_offset,
    )?;

    let ldso_elf_base = process_vm.get_elf_ranges()[1].start() as u64;
    auxvec.set(AuxKey::AT_BASE, ldso_elf_base)?;

    let syscall_addr = __syscall_entry_linux_abi as *const () as u64;
    auxvec.set(AuxKey::AT_OCCLUM_ENTRY, syscall_addr)?;
    // TODO: init AT_EXECFN
    // auxvec.set_val(AuxKey::AT_EXECFN, "program_name")?;

    Ok(auxvec)
}

extern "C" {
    fn __syscall_entry_linux_abi() -> i64;
    fn occlum_gdb_hook_load_elf(elf_base: u64, elf_path: *const u8, elf_path_len: u64);
}
