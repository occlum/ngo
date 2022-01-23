use super::builtin_disk::try_open_disk;
use super::*;
use crate::fs::DiskFile;

pub fn do_openat(fs_path: &FsPath, flags: u32, mode: FileMode) -> Result<FileDesc> {
    debug!(
        "openat: fs_path: {:?}, flags: {:#o}, mode: {:#o}",
        fs_path, flags, mode
    );

    let current = current!();
    let fs = current.fs().read().unwrap();
    let masked_mode = mode & !current.process().umask();

    let file_ref = if let Some(disk_file) = try_open_disk(&fs, fs_path)? {
        FileRef::new_disk(disk_file)
    } else {
        let inode_file = fs.open_file(&fs_path, flags, masked_mode)?;
        FileRef::new_inode(inode_file)
    };

    let fd = {
        let creation_flags = CreationFlags::from_bits_truncate(flags);
        current.add_file(file_ref, creation_flags.must_close_on_spawn())
    };
    Ok(fd)
}
