use super::{FileType, Metadata, Timespec};

#[repr(C)]
pub struct StatBuf {
    /// ID of device containing file
    pub dev: u64,
    /// inode number
    pub ino: u64,
    /// number of hard links
    pub nlink: u64,

    /// file type and mode
    pub mode: StatMode,
    /// user ID of owner
    pub uid: u32,
    /// group ID of owner
    pub gid: u32,
    /// padding
    pub _pad0: u32,
    /// device ID (if special file)
    pub rdev: u64,
    /// total size, in bytes
    pub size: u64,
    /// blocksize for filesystem I/O
    pub blksize: u64,
    /// number of 512B blocks allocated
    pub blocks: u64,

    /// last access time
    pub atime: Timespec,
    /// last modification time
    pub mtime: Timespec,
    /// last status change time
    pub ctime: Timespec,
}

impl Default for StatBuf {
    fn default() -> Self {
        Self {
            dev: 0,
            ino: 0,
            mode: StatMode::NULL,
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            blksize: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            _pad0: 0,
        }
    }
}

impl From<Metadata> for StatBuf {
    fn from(info: Metadata) -> Self {
        StatBuf {
            dev: info.dev as u64,
            ino: info.inode as u64,
            mode: StatMode::from_type_and_mode(info.type_, info.mode),
            nlink: info.nlinks as u64,
            uid: info.uid as u32,
            gid: info.gid as u32,
            rdev: 0,
            size: info.size as u64,
            blksize: info.blk_size as u64,
            blocks: info.blocks as u64,
            atime: info.atime,
            mtime: info.mtime,
            ctime: info.ctime,
            _pad0: 0,
        }
    }
}

bitflags::bitflags! {
    pub struct StatMode: u32 {
        const NULL  = 0;
        /// Type
        const TYPE_MASK = 0o170000;
        /// FIFO
        const FIFO  = 0o010000;
        /// character device
        const CHAR  = 0o020000;
        /// directory
        const DIR   = 0o040000;
        /// block device
        const BLOCK = 0o060000;
        /// ordinary regular file
        const FILE  = 0o100000;
        /// symbolic link
        const LINK  = 0o120000;
        /// socket
        const SOCKET = 0o140000;

        /// Set-user-ID on execution.
        const SET_UID = 0o4000;
        /// Set-group-ID on execution.
        const SET_GID = 0o2000;

        /// Read, write, execute/search by owner.
        const OWNER_MASK = 0o700;
        /// Read permission, owner.
        const OWNER_READ = 0o400;
        /// Write permission, owner.
        const OWNER_WRITE = 0o200;
        /// Execute/search permission, owner.
        const OWNER_EXEC = 0o100;

        /// Read, write, execute/search by group.
        const GROUP_MASK = 0o70;
        /// Read permission, group.
        const GROUP_READ = 0o40;
        /// Write permission, group.
        const GROUP_WRITE = 0o20;
        /// Execute/search permission, group.
        const GROUP_EXEC = 0o10;

        /// Read, write, execute/search by others.
        const OTHER_MASK = 0o7;
        /// Read permission, others.
        const OTHER_READ = 0o4;
        /// Write permission, others.
        const OTHER_WRITE = 0o2;
        /// Execute/search permission, others.
        const OTHER_EXEC = 0o1;
    }
}

impl StatMode {
    pub fn from_type_and_mode(type_: FileType, mode: u16) -> Self {
        let type_ = match type_ {
            FileType::File => StatMode::FILE,
            FileType::Dir => StatMode::DIR,
            FileType::SymLink => StatMode::LINK,
            FileType::CharDevice => StatMode::CHAR,
            FileType::BlockDevice => StatMode::BLOCK,
            FileType::Socket => StatMode::SOCKET,
            FileType::NamedPipe => StatMode::FIFO,
            _ => StatMode::NULL,
        };
        let mode = StatMode::from_bits_truncate(mode as u32);
        type_ | mode
    }
}

bitflags::bitflags! {
    pub struct StatFlags: u32 {
        const AT_EMPTY_PATH = 1 << 12;
        const AT_NO_AUTOMOUNT = 1 << 11;
        const AT_SYMLINK_NOFOLLOW = 1 << 8;
    }
}
