use std::collections::{HashMap, VecDeque};
use std::sync::Weak;

use new_self_ref_arc::new_self_ref_arc;

use super::{EpollCtl, EpollEntry, EpollEvent, EpollFlags};
use crate::fs::{AccessMode, Events, Observer, Pollee, Poller, StatusFlags};
use crate::prelude::*;

/// A file-like object that provides epoll API.
///
/// Conceptually, we maintain two lists: one consists of all interesting files,
/// which can be managed by the epoll ctl commands; the other are for ready files,
/// which are files that have some events. A epoll wait only needs to iterate the
/// ready list and poll each file to see if the file is ready for the interesting
/// I/O.
///
/// To maintain the ready list, we need to monitor interesting events that happen
/// on the files. To do so, the `EpollFile` registers itself as an `Observer` to
/// the monotored files. Thus, we can add a file to the ready list when an interesting
/// event happens on the file.
pub struct EpollFile {
    // All interesting entries.
    interest: SgxMutex<HashMap<FileDesc, Arc<EpollEntry>>>,
    // Entries that are probably ready (having events happened).
    ready: SgxMutex<VecDeque<Arc<EpollEntry>>>,
    // EpollFile itself is also pollable
    pollee: Pollee,
    // Any EpollFile is wrapped with Arc when created.
    weak_self: Weak<Self>,
}

impl EpollFile {
    /// Creates a new epoll file.
    ///
    /// An `EpollFile` is always contained inside `Arc`.
    pub fn new() -> Arc<Self> {
        let new_self = Self {
            interest: Default::default(),
            ready: Default::default(),
            pollee: Pollee::new(Events::empty()),
            weak_self: Weak::new(),
        };
        new_self_ref_arc!(new_self)
    }

    /// Control the interest list of the epoll file.
    pub fn control(&self, cmd: &EpollCtl) -> Result<()> {
        match *cmd {
            EpollCtl::Add(fd, ep_event, ep_flags) => self.add_interest(fd, ep_event, ep_flags),
            EpollCtl::Del(fd) => self.del_interest(fd),
            EpollCtl::Mod(fd, ep_event, ep_flags) => self.mod_interest(fd, ep_event, ep_flags),
        }
    }

    fn add_interest(&self, fd: FileDesc, ep_event: EpollEvent, ep_flags: EpollFlags) -> Result<()> {
        self.warn_unsupported_flags(&ep_flags);

        let file = current!().file(fd)?;
        let weak_file = FileRef::downgrade(&file);
        let mask = ep_event.events;
        let entry = EpollEntry::new(fd, weak_file, ep_event, ep_flags, self.weak_self.clone());

        // Add the new entry to the interest list and start monitering its events
        let mut interest = self.interest.lock().unwrap();
        if interest.contains_key(&fd) {
            return_errno!(EEXIST, "the fd has been added");
        }
        file.register_observer(entry.clone(), Events::all())?;
        interest.insert(fd, entry.clone());
        drop(interest);

        // Add the new entry to the ready list if the file is ready
        let events = file.poll(mask, None);
        if !events.is_empty() {
            self.push_ready(entry);
        }
        Ok(())
    }

    fn del_interest(&self, fd: FileDesc) -> Result<()> {
        let mut interest = self.interest.lock().unwrap();
        let entry = interest
            .remove(&fd)
            .ok_or_else(|| errno!(ENOENT, "fd is not in the interest list"))?;

        // If this epoll entry is in the ready list, then we should delete it.
        // But unfortunately, deleting an entry from the ready list has a
        // complexity of O(N).
        //
        // To optimize the performance, we only mark the epoll entry as
        // deleted at this moment. The real deletion happens when the ready list
        // is scanned in EpolFile::wait.
        entry.set_deleted();

        let file = match entry.file() {
            Some(file) => file,
            // TODO: should we warn about it?
            None => return Ok(()),
        };
        file.unregister_observer(&(entry as _)).unwrap();
        Ok(())
    }

    fn mod_interest(
        &self,
        fd: FileDesc,
        new_ep_event: EpollEvent,
        new_ep_flags: EpollFlags,
    ) -> Result<()> {
        self.warn_unsupported_flags(&new_ep_flags);

        // Update the epoll entry
        let interest = self.interest.lock().unwrap();
        let entry = interest
            .get(&fd)
            .ok_or_else(|| errno!(ENOENT, "fd is not in the interest list"))?;
        if entry.is_deleted() {
            return_errno!(ENOENT, "fd is not in the interest list");
        }
        let new_mask = new_ep_event.events;
        entry.update(new_ep_event, new_ep_flags);
        let entry = entry.clone();
        drop(interest);

        // Add the updated entry to the ready list if the file is ready
        let file = match entry.file() {
            Some(file) => file,
            None => return Ok(()),
        };
        let events = file.poll(new_mask, None);
        if !events.is_empty() {
            self.push_ready(entry);
        }
        Ok(())
    }

    /// Wait for interesting events happen on the files in the interest list
    /// of the epoll file.
    ///
    /// This method blocks until either some interesting events happen or
    /// the timeout expires or a signal arrives. The first case returns
    /// `Ok(events)`, where `events` is a `Vec` containing at most `max_events`
    /// number of `EpollEvent`s. The second and third case returns errors.
    ///
    /// When `max_events` equals to zero, the method returns when the timeout
    /// expires or a signal arrives.
    pub async fn wait(
        &self,
        max_events: usize,
        //timeout: Option<&mut Duration>,
    ) -> Result<Vec<EpollEvent>> {
        let mut ep_events = Vec::new();
        let mut poller = None;
        loop {
            // Try to pop some ready entries
            if self.pop_ready(max_events, &mut ep_events) > 0 {
                return Ok(ep_events);
            }

            // If no ready entries for now, wait for them
            if poller.is_none() {
                poller = Some(Poller::new());
            }
            poller.as_ref().unwrap().wait().await;
        }
    }

    fn push_ready(&self, entry: Arc<EpollEntry>) {
        let mut ready = self.ready.lock().unwrap();
        if entry.is_ready() || entry.is_deleted() {
            return;
        }
        entry.set_ready();
        ready.push_back(entry);

        self.pollee.add_events(Events::IN);
    }

    fn pop_ready(&self, max_events: usize, ep_events: &mut Vec<EpollEvent>) -> usize {
        let mut count_events = 0;
        let mut ready = self.ready.lock().unwrap();
        let mut pop_quota = ready.len();
        loop {
            // Pop some ready entries per round.
            //
            // Since the popped ready entries may contain "false positive" and
            // we want to return as many results as possible, this has to
            // be done in a loop.
            let pop_count = (max_events - count_events).min(pop_quota);
            let ready_entries: Vec<Arc<EpollEntry>> = ready
                .drain(..pop_count)
                .filter(|entry| !entry.is_deleted())
                .collect();
            pop_quota -= pop_count;

            // Examine these ready entries, which are candidates for the results
            // to be returned.
            for entry in ready_entries {
                let (ep_event, ep_flags) = entry.event_and_flags();

                // If this entry's file is ready, save it in the output array.
                let ready_events = entry.poll() & ep_event.events;
                if !ready_events.is_empty() {
                    ep_events.push(ep_event);
                    count_events += 1;
                }

                // If the epoll entry is neither edge-triggered or one-shot, then we should
                // keep the entry in the ready list.
                if !ep_flags.intersects(EpollFlags::ONE_SHOT | EpollFlags::EDGE_TRIGGER) {
                    ready.push_back(entry);
                }
                // Otherwise, the entry is indeed removed the ready list and we should reset
                // its ready flag.
                else {
                    entry.reset_ready();
                }
            }
        }

        // Clear the epoll file's events if no ready entries
        if ready.len() == 0 {
            self.pollee.del_events(Events::IN);
        }
        count_events
    }

    fn warn_unsupported_flags(&self, flags: &EpollFlags) {
        if flags.intersects(EpollFlags::EXCLUSIVE | EpollFlags::WAKE_UP) {
            warn!("{:?} contains unsupported flags", flags);
        }
    }
}

impl Observer for EpollEntry {
    fn on_events(&self, _pollee_id: u64, _events: Events) {
        // Fast path
        if self.is_ready() || self.is_deleted() {
            return;
        }

        let epoll_file = self.epoll_file();
        epoll_file.push_ready(self.self_arc());
    }
}

impl std::fmt::Debug for EpollFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpollFile")
            .field("interest", &*self.interest.lock().unwrap())
            .field("ready", &*self.ready.lock().unwrap())
            .field("pollee", &self.pollee)
            .finish()
    }
}

// Implement the common methods required by FileHandle
impl EpollFile {
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        return_errno!(EINVAL, "epoll files do not support read");
    }

    pub async fn readv(&self, bufs: &mut [&mut [u8]]) -> Result<usize> {
        return_errno!(EINVAL, "epoll files do not support read");
    }

    pub async fn write(&self, buf: &[u8]) -> Result<usize> {
        return_errno!(EINVAL, "epoll files do not support write");
    }

    pub async fn writev(&self, bufs: &[&[u8]]) -> Result<usize> {
        return_errno!(EINVAL, "epoll files do not support write");
    }

    pub fn access_mode(&self) -> AccessMode {
        // We consider all epoll files read-only
        AccessMode::O_RDONLY
    }

    pub fn status_flags(&self) -> StatusFlags {
        StatusFlags::empty()
    }

    pub fn set_status_flags(&self, new_flags: StatusFlags) -> Result<()> {
        return_errno!(EINVAL, "epoll files do not support set_status_flags");
    }

    pub fn poll(&self, mask: Events, poller: Option<&mut Poller>) -> Events {
        self.pollee.poll(mask, poller)
    }

    pub fn register_observer(&self, observer: Arc<dyn Observer>, mask: Events) -> Result<()> {
        self.pollee.register_observer(observer, mask);
        Ok(())
    }

    pub fn unregister_observer(&self, observer: &Arc<dyn Observer>) -> Result<Arc<dyn Observer>> {
        self.pollee
            .unregister_observer(observer)
            .ok_or_else(|| errno!(EINVAL, "observer is not registered"))
    }
}
