use core::borrow::BorrowMut;
use core::task::Waker as RawWaker;
use futures::select_biased;

use atomic::{Atomic, Ordering};
use intrusive_collections::LinkedListLink;
use object_id::ObjectId;

use crate::prelude::*;
use crate::task::current;
use crate::time::{TimerEntry, TimerFutureEntry, DURATION_ZERO};

/// A waiter.
///
/// `Waiter`s are mostly used with `WaiterQueue`s. Yet, it is also possible to
/// use `Waiter` with `Waker`.
pub struct Waiter {
    inner: Arc<WaiterInner>,
}

/// The states of a waiter.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WaiterState {
    Idle,
    Waiting,
    Woken,
}

impl Waiter {
    pub fn new() -> Self {
        let inner = Arc::new(WaiterInner::new());
        Self { inner }
    }

    pub fn reset(&self) {
        self.inner.shared_state.lock().state = WaiterState::Idle
    }

    /// Wait until being woken by the waker.
    pub async fn wait(&self) -> Result<()> {
        let task = current::get().clone();
        let signal = task.signal();

        if signal.is_none() {
            self.inner.wait().await;
            return Ok(());
        }

        let signal = signal.clone().unwrap();

        if let Some(mut waiter) = signal.waiter() {
            select_biased! {
                _ = self.inner.wait().fuse() => {
                    signal.unregister(&mut waiter);
                    return Ok(());
                }
                _ = waiter.inner.wait().fuse() => {
                    signal.unregister(&mut waiter);
                    return_errno!(EINTR, "the waiter interrupted");
                }
            }
        } else {
            return_errno!(EINTR, "the waiter interrupted");
        }
    }

    /// Wait until being woken by the waker or reaching timeout.
    ///
    /// In each poll, we will first poll a `WaitFuture` object, if the result is `Ready`, return `Ok`.
    /// If the result is `Pending`, we will poll a `TimerEntry` object, return `Err` if got `Ready`.
    pub async fn wait_timeout<T: BorrowMut<Duration>>(
        &self,
        timeout: Option<&mut T>,
    ) -> Result<()> {
        match timeout {
            Some(t) => {
                let timer_entry = TimerEntry::new(*t.borrow_mut());
                select_biased! {
                    ret = self.wait().fuse() => {
                        // We wake up before timeout expired.
                        *t.borrow_mut() = timer_entry.remained_duration();
                        return ret;
                    }
                    _ = TimerFutureEntry::new(&timer_entry).fuse() => {
                        // The timer expired, we reached timeout.
                        *t.borrow_mut() = DURATION_ZERO;
                        return_errno!(ETIMEDOUT, "the waiter reached timeout");
                    }
                };
            }
            None => {
                return self.wait().await;
            }
        };
    }

    pub fn waker(&self) -> Waker {
        Waker {
            inner: self.inner.clone(),
        }
    }

    pub(super) fn inner(&self) -> &Arc<WaiterInner> {
        &self.inner
    }
}

#[derive(Clone)]
pub struct Waker {
    inner: Arc<WaiterInner>,
}

impl Waker {
    pub fn state(&self) -> WaiterState {
        self.inner.state()
    }

    pub fn wake(&self) -> Option<()> {
        self.inner.wake()
    }
}

// Note: state and waker must be updated together.
struct SharedState {
    state: WaiterState,
    raw_waker: Option<RawWaker>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            state: WaiterState::Idle,
            raw_waker: None,
        }
    }
}

// Accesible by WaiterQueue.
pub(super) struct WaiterInner {
    shared_state: Mutex<SharedState>,
    queue_id: Atomic<ObjectId>,
    pub(super) link: LinkedListLink,
}

impl WaiterInner {
    pub fn new() -> Self {
        Self {
            shared_state: Mutex::new(SharedState::new()),
            link: LinkedListLink::new(),
            queue_id: Atomic::new(ObjectId::null()),
        }
    }

    pub fn state(&self) -> WaiterState {
        self.shared_state.lock().state.clone()
    }

    pub fn queue_id(&self) -> &Atomic<ObjectId> {
        &self.queue_id
    }

    pub fn wait(&self) -> WaitFuture<'_> {
        WaitFuture::new(self)
    }

    pub fn wake(&self) -> Option<()> {
        let mut shared_state = self.shared_state.lock();
        match shared_state.state {
            WaiterState::Idle => {
                shared_state.state = WaiterState::Woken;
                Some(())
            }
            WaiterState::Waiting => {
                shared_state.state = WaiterState::Woken;
                let raw_waker = shared_state.raw_waker.take().unwrap();
                raw_waker.wake();
                Some(())
            }
            WaiterState::Woken => None,
        }
    }
}

unsafe impl Sync for WaiterInner {}
unsafe impl Send for WaiterInner {}

pub struct WaitFuture<'a> {
    waiter: &'a WaiterInner,
}

impl<'a> WaitFuture<'a> {
    fn new(waiter: &'a WaiterInner) -> Self {
        Self { waiter }
    }
}

impl<'a> Future for WaitFuture<'a> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut shared_state = self.waiter.shared_state.lock();
        match shared_state.state {
            WaiterState::Idle => {
                shared_state.state = WaiterState::Waiting;
                shared_state.raw_waker = Some(cx.waker().clone());
                Poll::Pending
            }
            WaiterState::Waiting => {
                shared_state.raw_waker = Some(cx.waker().clone());
                Poll::Pending
            }
            WaiterState::Woken => {
                debug_assert!(shared_state.raw_waker.is_none());
                Poll::Ready(())
            }
        }
    }
}

impl<'a> Drop for WaitFuture<'a> {
    fn drop(&mut self) {
        let mut shared_state = self.waiter.shared_state.lock();
        if let WaiterState::Waiting = shared_state.state {
            shared_state.raw_waker = None;
            shared_state.state = WaiterState::Idle;
        }
    }
}
