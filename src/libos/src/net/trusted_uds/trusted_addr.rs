use super::*;
use crate::fs::FsPath;
use crate::fs::{CreationFlags, EventFileFlags, FileMode};
use crate::util::sync::*;
use std::any::Any;
use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::{cmp, mem, slice, str};

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct TrustedAddr {
    unix_addr: UnixAddr,
    inode: Option<usize>, // If unix_addr is a real file name, there will be corresponding inode number
}

impl TrustedAddr {
    pub fn inner(&self) -> &UnixAddr {
        &self.unix_addr
    }

    pub fn inode(&self) -> Option<usize> {
        self.inode
    }

    // Bind the unix address with the inode of the FS
    pub fn bind_addr(&mut self) -> Result<()> {
        if let UnixAddr::Pathname(path) = &self.unix_addr {
            let inode_num = {
                let current = current!();
                let fs = current.fs().read().unwrap();
                let file_ref = fs.open_file(
                    &FsPath::try_from(path.as_ref())?,
                    CreationFlags::O_CREAT.bits(),
                    FileMode::from_bits(0o777).unwrap(),
                )?;
                file_ref.inode().metadata()?.inode
            };
            self.inode = Some(inode_num);
        }
        Ok(())
    }
}

impl Addr for TrustedAddr {
    fn domain() -> Domain {
        Domain::Unix
    }

    fn from_c_storage(c_addr: &libc::sockaddr_storage, c_addr_len: usize) -> Result<Self> {
        Ok(Self {
            unix_addr: UnixAddr::from_c_storage(c_addr, c_addr_len)?,
            inode: None,
        })
    }

    fn to_c_storage(&self) -> (libc::sockaddr_storage, usize) {
        self.unix_addr.to_c_storage()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
