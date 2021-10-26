use super::*;

pub fn do_unlinkat(fs_path: &FsPath, flags: UnlinkFlags) -> Result<()> {
    debug!("unlinkat: fs_path: {:?}, flags: {:?}", fs_path, flags);

    if flags.contains(UnlinkFlags::AT_REMOVEDIR) {
        super::do_rmdir(fs_path)
    } else {
        do_unlink(fs_path)
    }
}

bitflags::bitflags! {
    pub struct UnlinkFlags: i32 {
        const AT_REMOVEDIR = 0x200;
    }
}

fn do_unlink(fs_path: &FsPath) -> Result<()> {
    let (dir_inode, file_name) = {
        let current = current!();
        let fs = current.fs().lock().unwrap();
        fs.lookup_dirinode_and_basename(fs_path)?
    };
    let file_inode = dir_inode.find(&file_name)?;
    let metadata = file_inode.metadata()?;
    if metadata.type_ == FileType::Dir {
        return_errno!(EISDIR, "unlink on directory");
    }
    let file_mode = FileMode::from_bits_truncate(metadata.mode);
    if file_mode.has_sticky_bit() {
        warn!("ignoring the sticky bit");
    }
    dir_inode.unlink(&file_name)?;
    Ok(())
}
