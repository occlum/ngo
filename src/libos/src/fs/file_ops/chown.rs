use super::*;

bitflags! {
    pub struct ChownFlags: i32 {
        const AT_EMPTY_PATH = 0x1000;
        const AT_SYMLINK_NOFOLLOW = 0x100;
    }
}

pub fn do_fchownat(fs_path: &FsPath, uid: u32, gid: u32, flags: ChownFlags) -> Result<()> {
    debug!(
        "fchownat: fs_path: {:?}, uid: {}, gid: {}, flags: {:?}",
        fs_path, uid, gid, flags
    );

    let inode = {
        let current = current!();
        let fs = current.fs().lock().unwrap();
        if flags.contains(ChownFlags::AT_SYMLINK_NOFOLLOW) {
            fs.lookup_inode_no_follow(fs_path)?
        } else {
            fs.lookup_inode(fs_path)?
        }
    };
    let mut info = inode.metadata()?;
    info.uid = uid as usize;
    info.gid = gid as usize;
    inode.set_metadata(&info)?;
    Ok(())
}

pub fn do_fchown(fd: FileDesc, uid: u32, gid: u32) -> Result<()> {
    debug!("fchown: fd: {}, uid: {}, gid: {}", fd, uid, gid);

    let file_ref = current!().file(fd)?;
    let inode_file = file_ref
        .as_inode_file()
        .ok_or_else(|| errno!(EINVAL, "not an inode"))?;
    let inode = inode_file.inode();
    let mut info = inode.metadata()?;
    info.uid = uid as usize;
    info.gid = gid as usize;
    inode.set_metadata(&info)?;
    Ok(())
}
