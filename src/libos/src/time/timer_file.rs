use super::*;
use crate::fs::{AccessMode, Events, IoctlCmd, Observer, Pollee, Poller, StatusFlags};
use async_rt::task::{JoinHandle, SpawnOptions};
use atomic::{Atomic, Ordering};
use std::any::Any;
use std::sync::mpsc;
use std::time::Duration;
use untrusted::{SliceAsMutPtrAndLen, SliceAsPtrAndLen};

#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum TimerFdStatus {
    ONE_SHOT = 0,
    LOOP = 1,
    STOP = 2,
}

#[derive(Debug)]
pub struct TimerFile {
    clockid: ClockId,
    flags: Atomic<TimerCreationFlags>,
    inner: SgxMutex<TimerInner>,
    pollee: Pollee,
}

#[derive(Debug)]
pub struct TimerInner {
    fire_time: Duration,
    cur_interval: Duration,
    alarm_cnt: usize,
    last_alarm_cnt: usize,
    status: TimerFdStatus,
    task_handle: Option<JoinHandle<()>>,
}

impl TimerInner {
    pub fn new() -> Self {
        Self {
            fire_time: Duration::default(),
            cur_interval: Duration::default(),
            alarm_cnt: 0,
            last_alarm_cnt: 0,
            status: TimerFdStatus::STOP,
            task_handle: None,
        }
    }
}

impl Drop for TimerFile {
    fn drop(&mut self) {
        trace!("TimerFile Drop");
        let mut inner = self.inner.lock().unwrap();
        if inner.task_handle.is_some() {
            inner
                .task_handle
                .as_ref()
                .unwrap()
                .task()
                .tirqs()
                .put_req(0);
        }
    }
}

impl TimerFile {
    pub fn new(clockid: ClockId, flags: TimerCreationFlags) -> Result<Self> {
        Ok(Self {
            clockid,
            flags: Atomic::new(flags),
            inner: SgxMutex::new(TimerInner::new()),
            pollee: Pollee::new(Events::empty()),
        })
    }

    pub fn set_time(
        &self,
        flags: TimerSetFlags,
        new_value: &TimerfileDurations,
    ) -> Result<TimerfileDurations> {
        let cur_time =
            timespec_t::from(vdso_time::clock_gettime(self.clockid).unwrap()).as_duration();

        let mut inner = self.inner.lock().unwrap();
        let old_it_value = inner.fire_time;
        let old_it_interval = inner.cur_interval;

        if new_value.it_value.is_zero() == true {
            debug!("TimerFd: stop timer");
            inner.status = TimerFdStatus::STOP;
            if inner.task_handle.is_some() {
                inner
                    .task_handle
                    .as_ref()
                    .unwrap()
                    .task()
                    .tirqs()
                    .put_req(0);
            }

            inner.task_handle = None;

            return Ok(TimerfileDurations {
                it_interval: old_it_interval,
                it_value: old_it_value,
            });
        }

        let fire_time = match flags {
            TimerSetFlags::TFD_TIMER_ABSTIME => new_value.it_value,
            _ => cur_time.checked_add(new_value.it_value).unwrap(),
        };

        debug!("TimerFd: start timer");
        inner.cur_interval = new_value.it_interval;
        inner.fire_time = fire_time;

        if new_value.it_interval.is_zero() == true {
            inner.status = TimerFdStatus::ONE_SHOT;
        } else {
            inner.status = TimerFdStatus::LOOP;
        }

        inner.alarm_cnt = 0;
        inner.last_alarm_cnt = 0;

        // Start background poll task to monitor timerfd events
        let join_handle = SpawnOptions::new({
            let pollee = self.pollee.clone();
            let mut it_value = match inner.fire_time.checked_sub(cur_time) {
                Some(duration) => duration,
                _ => Duration::new(0, 0),
            };

            let interval = inner.cur_interval;

            async move {
                let waiter = Waiter::new();
                if let Ok(_) = waiter.wait_timeout(Some(&mut it_value)).await {
                    // TODO: support interrupt sleep.
                    // return_errno!(EINTR, "sleep interrupted");
                    unreachable!("this waiter can not be interrupted");
                }

                trace!("timerfd trigger it_value");
                pollee.add_events(Events::IN);

                if !interval.is_zero() {
                    loop {
                        let mut timeout = interval;
                        let waiter = Waiter::new();
                        let res = waiter.wait_timeout(Some(&mut timeout)).await;
                        match res {
                            Err(e) => {
                                if e.errno() == EINTR {
                                    break;
                                } else {
                                    trace!("timerfd trigger it_interval {:?}", interval);
                                    pollee.add_events(Events::IN);
                                }
                            }
                            _ => {
                                panic!("impossible as there is no waker or timeout");
                            }
                        }
                    }
                }

                trace!("Timerfd poll task end");
            }
        })
        .priority(async_rt::sched::SchedPriority::Low)
        .spawn();

        inner.task_handle = Some(join_handle);

        Ok(TimerfileDurations {
            it_interval: old_it_interval,
            it_value: old_it_value,
        })
    }

    pub fn time(&self) -> Result<TimerfileDurations> {
        let mut ret_time = TimerfileDurations::default();
        let mut inner = self.inner.lock().unwrap();

        let interval = inner.cur_interval;
        let cur_time =
            timespec_t::from(vdso_time::clock_gettime(self.clockid).unwrap()).as_duration();

        if inner.status != TimerFdStatus::STOP {
            let delta = match inner.fire_time.checked_sub(cur_time) {
                Some(duration) => duration,
                _ => Duration::new(0, 0),
            };

            if inner.status == TimerFdStatus::ONE_SHOT {
                if delta.is_zero() == true {
                    inner.alarm_cnt = 1;
                    inner.status = TimerFdStatus::STOP;
                }

                ret_time.it_value = delta;
            } else {
                // loop mode
                if delta.is_zero() == false {
                    // still in the initial it_value alarm range
                    ret_time.it_value = delta;
                } else {
                    // In the loop interval range
                    let delta = match cur_time.checked_sub(inner.fire_time) {
                        Some(duration) => duration,
                        _ => Duration::new(0, 0),
                    };

                    let div = delta.div_duration_f32(interval).ceil() as u32;
                    let remaining = match interval.checked_mul(div).unwrap().checked_sub(delta) {
                        Some(duration) => duration,
                        _ => Duration::new(0, 0),
                    };

                    inner.alarm_cnt = div as usize;
                    ret_time.it_value = remaining;
                }
            }
        }

        ret_time.it_interval = interval;
        Ok(ret_time)
    }

    fn get_alarm_cnt(&self) -> (usize, Duration) {
        let time = self.time();
        let mut inner = self.inner.lock().unwrap();

        let alarm_cnt = inner.alarm_cnt;
        let last_cnt = inner.last_alarm_cnt;
        let mut cnt = 0;
        if alarm_cnt > last_cnt {
            cnt = alarm_cnt - last_cnt;
        }

        (cnt, time.unwrap().it_value)
    }

    async fn wait_for_alarm(&self) -> Result<usize> {
        let waiter = Waiter::new();
        let mut count: usize = 0;
        let mut left = Duration::new(0, 0);

        loop {
            let flags = self.flags.load(Ordering::Relaxed);
            let alarm = self.get_alarm_cnt();
            count = alarm.0;
            left = alarm.1;

            if count > 0 {
                break;
            } else if flags.contains(TimerCreationFlags::TFD_NONBLOCK) {
                return_errno!(EAGAIN, "try again");
            } else {
                // block read, do wait for left time to rest cpu
                if let Ok(_) = waiter.wait_timeout(Some(&mut left)).await {
                    // TODO: support interrupt sleep.
                    // return_errno!(EINTR, "sleep interrupted");
                    unreachable!("this waiter can not be interrupted");
                }
            }
        }

        Ok(count)
    }
}

bitflags! {
    pub struct TimerCreationFlags: i32 {
        /// Provides semaphore-like semantics for reads from the new file descriptor
        /// Non-blocking
        const TFD_NONBLOCK  = 1 << 11;
        /// Close on exec
        const TFD_CLOEXEC   = 1 << 19;
    }
}

bitflags! {
    pub struct TimerSetFlags: i32 {
        const TFD_TIMER_ABSTIME = 1 << 0;
        const TFD_TIMER_CANCEL_ON_SET = 1 << 1;
    }
}

impl TimerFile {
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        if buf.len() < 8 {
            return_errno!(EINVAL, "buffer is too small");
        }

        let mut inner = self.inner.lock().unwrap();

        if inner.status == TimerFdStatus::STOP {
            errno!(EAGAIN, "try again");
            // to return -1
            return Ok(usize::MAX);
        }

        SgxMutex::unlock(inner);

        let buf = &mut buf[0..8];
        let mut count = 0;
        match self.wait_for_alarm().await {
            Ok(size) => count = size,
            Err(_) => return_errno!(EAGAIN, "try again"),
        }

        let mut inner = self.inner.lock().unwrap();

        // Update alarm count
        inner.last_alarm_cnt = inner.alarm_cnt;

        let bytes = count.to_ne_bytes();
        buf.copy_from_slice(&bytes);

        self.pollee.del_events(Events::IN);

        Ok(8)
    }

    pub async fn readv(&self, bufs: &mut [&mut [u8]]) -> Result<usize> {
        return_errno!(EINVAL, "timer fds do not support readv");
    }

    pub async fn write(&self, buf: &[u8]) -> Result<usize> {
        return_errno!(EINVAL, "timer fds do not support write");
    }

    pub async fn writev(&self, bufs: &[&[u8]]) -> Result<usize> {
        return_errno!(EINVAL, "timer fds do not support write");
    }

    pub fn access_mode(&self) -> AccessMode {
        // We consider all timer fds read-only
        AccessMode::O_RDONLY
    }

    pub fn status_flags(&self) -> StatusFlags {
        let flags = self.flags.load(Ordering::Relaxed);

        if flags.contains(TimerCreationFlags::TFD_NONBLOCK) {
            StatusFlags::O_NONBLOCK
        } else {
            StatusFlags::empty()
        }
    }

    pub fn set_status_flags(&self, new_flags: StatusFlags) -> Result<()> {
        if new_flags.is_nonblocking() {
            self.flags
                .store(TimerCreationFlags::TFD_NONBLOCK, Ordering::Relaxed);
        } else {
            self.flags
                .store(TimerCreationFlags::empty(), Ordering::Relaxed);
        }

        Ok(())
    }

    pub fn ioctl(&self, cmd: &mut dyn IoctlCmd) -> Result<()> {
        return_errno!(EINVAL, "timer fds do not support ioctl");
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
            .ok_or_else(|| errno!(ENOENT, "the observer is not registered"))
    }
}
