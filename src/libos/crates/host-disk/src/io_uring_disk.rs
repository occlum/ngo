use block_device::{BioReq, BioSubmission, BioType, BlockDevice};
use io_uring_callback::{Fd, IoHandle, IoUring};
use std::fs::File;
use std::io::prelude::*;
use std::marker::PhantomData;
use std::os::unix::io::{AsRawFd, RawFd};

use crate::prelude::*;
use crate::{HostDisk, OpenOptions};

/// Providing an io_uring instance to be used by IoUringDisk.
///
/// This trait is introduced to decouple the creation of io_uring from
/// its users.
pub trait IoUringProvider: Send + Sync + 'static {
    fn io_uring() -> &'static IoUring;
}

/// A type of host disk that implements a block device interface by performing
/// async I/O via Linux's io_uring.
pub struct IoUringDisk<P: IoUringProvider>(Arc<Inner>, PhantomData<P>);

struct Inner {
    fd: RawFd,
    file: Mutex<File>,
    total_blocks: usize,
    can_read: bool,
    can_write: bool,
}

impl<P: IoUringProvider> IoUringDisk<P> {
    fn read(&self, req: &Arc<BioReq>) -> Result<()> {
        if !self.0.can_read {
            return Err(errno!(EACCES, "read is not allowed"));
        }

        let (offset, _) = self.get_range_in_bytes(&req)?;

        let fd = Fd(self.0.fd as i32);
        let iovecs = req.access_mut_bufs_with(|bufs| {
            let iovecs: Vec<libc::iovec> = bufs
                .iter_mut()
                .map(|buf| {
                    let buf_slice = buf.as_slice_mut();
                    libc::iovec {
                        iov_base: buf_slice.as_mut_ptr() as _,
                        iov_len: BLOCK_SIZE,
                    }
                })
                .collect();

            // Note that it is necessary to wrap the Vec with Box. Otherwise,
            // the iovec_ptr will become invalid when the iovecs is moved into
            // the callback closure.
            Box::new(iovecs)
        });
        let iovecs_ptr = iovecs.as_ptr() as _;
        let iovecs_len = iovecs.len();
        let complete_fn = {
            let req = req.clone();
            // Safety. All pointers contained in iovecs are still valid as the
            // buffers of the BIO request is valid.
            let send_iovecs = unsafe { MarkSend::new(iovecs) };
            move |retval: i32| {
                // When the callback is invoked, the iovecs must have been
                // useless. And we call drop it safely.
                drop(send_iovecs);

                let resp = if retval >= 0 {
                    let expected_len = req.num_bufs() * BLOCK_SIZE;
                    // We don't expect short reads on regular files
                    assert!(retval as usize == expected_len);
                    Ok(())
                } else {
                    Err(Errno::from((-retval) as u32))
                };

                unsafe {
                    req.complete(resp);
                }
            }
        };
        let io_uring = P::io_uring();
        let io_handle = unsafe {
            io_uring.readv(
                fd,
                iovecs_ptr,
                iovecs_len as u32,
                offset as i64,
                0,
                complete_fn,
            )
        };
        // We don't need to keep the handle
        IoHandle::release(io_handle);

        Ok(())
    }

    fn write(&self, req: &Arc<BioReq>) -> Result<()> {
        if !self.0.can_write {
            return Err(errno!(EACCES, "write is not allowed"));
        }

        let (offset, _) = self.get_range_in_bytes(&req)?;

        let fd = Fd(self.0.fd as i32);
        let iovecs = req.access_bufs_with(|bufs| {
            let iovecs: Vec<libc::iovec> = bufs
                .iter()
                .map(|buf| {
                    let buf_slice = buf.as_slice();
                    libc::iovec {
                        iov_base: buf_slice.as_ptr() as *mut u8 as _,
                        iov_len: BLOCK_SIZE,
                    }
                })
                .collect();

            // Note that it is necessary to wrap the Vec with Box. Otherwise,
            // the iovec_ptr will become invalid when the iovecs is moved into
            // the callback closure.
            Box::new(iovecs)
        });
        let iovecs_ptr = iovecs.as_ptr() as _;
        let iovecs_len = iovecs.len();
        let complete_fn = {
            let req = req.clone();
            // Safety. All pointers contained in iovecs are still valid as the
            // buffers of the BIO request is valid.
            let send_iovecs = unsafe { MarkSend::new(iovecs) };
            move |retval: i32| {
                // When the callback is invoked, the iovecs must have been
                // useless. And we call drop it safely.
                drop(send_iovecs);

                let resp = if retval >= 0 {
                    let expected_len = req.num_bufs() * BLOCK_SIZE;
                    // We don't expect short writes on regular files
                    assert!(retval as usize == expected_len);
                    Ok(())
                } else {
                    Err(Errno::from((-retval) as u32))
                };

                unsafe {
                    req.complete(resp);
                }
            }
        };
        let io_uring = P::io_uring();
        let io_handle = unsafe {
            io_uring.writev(
                fd,
                iovecs_ptr,
                iovecs_len as u32,
                offset as i64,
                0,
                complete_fn,
            )
        };
        // We don't need to keep the handle
        IoHandle::release(io_handle);

        Ok(())
    }

    fn flush(&self, req: &Arc<BioReq>) -> Result<()> {
        if !self.0.can_write {
            return Err(errno!(EACCES, "flush is not allowed"));
        }

        let fd = Fd(self.0.fd as i32);
        let is_datasync = true;
        let complete_fn = {
            let req = req.clone();
            move |retval: i32| {
                let resp = if retval == 0 {
                    Ok(())
                } else if retval < 0 {
                    Err(Errno::from((-retval) as u32))
                } else {
                    panic!("impossible retval");
                };

                unsafe {
                    req.complete(resp);
                }
            }
        };
        let io_uring = P::io_uring();
        let io_handle = unsafe { io_uring.fsync(fd, is_datasync, complete_fn) };
        // We don't need to keep the handle
        IoHandle::release(io_handle);

        Ok(())
    }

    fn get_range_in_bytes(&self, req: &Arc<BioReq>) -> Result<(usize, usize)> {
        let begin_block = req.addr();
        let end_block = begin_block + req.num_bufs();
        if end_block > self.0.total_blocks {
            return Err(errno!(EINVAL, "invalid block range"));
        }
        let begin_offset = begin_block * BLOCK_SIZE;
        let end_offset = end_block * BLOCK_SIZE;
        Ok((begin_offset, end_offset))
    }
}

impl<P: IoUringProvider> BlockDevice for IoUringDisk<P> {
    fn total_blocks(&self) -> usize {
        self.0.total_blocks
    }

    fn submit(&self, req: Arc<BioReq>) -> BioSubmission {
        // Update the status of req to submittted
        let submission = BioSubmission::new(req);

        // Try to initiate the I/O
        let req = submission.req();
        let type_ = req.type_();
        let res = match type_ {
            BioType::Read => self.read(req),
            BioType::Write => self.write(req),
            BioType::Flush => self.flush(req),
        };

        // If any error returns, then the request must have failed to submit. So
        // we set its status of "completed" here and set the response to the error.
        if let Err(e) = res {
            unsafe {
                req.complete(Err(e.errno()));
            }
        }

        submission
    }
}

impl<P: IoUringProvider> HostDisk for IoUringDisk<P> {
    fn from_options_and_file(options: &OpenOptions<Self>, file: File) -> Result<Self> {
        let fd = file.as_raw_fd();
        let total_blocks = options.total_blocks.unwrap_or_else(|| {
            let file_len = file.metadata().unwrap().len() as usize;
            assert!(file_len >= BLOCK_SIZE);
            file_len / BLOCK_SIZE
        });
        let can_read = options.read;
        let can_write = options.write;
        let inner = Inner {
            fd,
            file: Mutex::new(file),
            total_blocks,
            can_read,
            can_write,
        };
        let new_self = Self(Arc::new(inner), PhantomData);
        Ok(new_self)
    }
}

impl<P: IoUringProvider> Drop for IoUringDisk<P> {
    fn drop(&mut self) {
        // Ensure all data are peristed before the disk is dropped
        let mut file = self.0.file.lock().unwrap();
        let _ = file.flush();
    }
}

/// Mark an instance of type `T` as `Send`.
///
/// This is useful when an instance of type `T` is safe to send across threads,
/// but the marker trait Send cannot be implemented for T due to the
/// orphan rules.
pub struct MarkSend<T>(T);

impl<T> MarkSend<T> {
    /// Wrap an instance of type `T` so that it becomes `Send`.
    ///
    /// # Safety
    ///
    /// The user must make sure that it is indeed to send such a value across
    /// threads.
    pub unsafe fn new(inner: T) -> Self {
        Self(inner)
    }
}

unsafe impl<T> Send for MarkSend<T> {}
