use super::*;
use crate::fs::{AccessMode, Events, IoctlCmd, Observer, Pollee, Poller, StatusFlags};
use async_rt::sched::yield_;
use atomic::{Atomic, Ordering};
use flume::{Receiver, Sender};
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
pub struct TimerFd {
    clockid: ClockId,
    flags: Atomic<TimerCreationFlags>,
    fire_time: Atomic<Duration>,
    cur_interval: Atomic<Duration>,
    alarm_cnt: Atomic<usize>,
    last_alarm_cnt: Atomic<usize>,
    status: Atomic<TimerFdStatus>,
    pollee: Pollee,
}

impl TimerFd {
    pub fn new(clockid: ClockId, flags: TimerCreationFlags) -> Result<Self> {
        Ok(Self {
            clockid,
            flags: Atomic::new(flags),
            fire_time: Atomic::new(Duration::default()),
            cur_interval: Atomic::new(Duration::default()),
            alarm_cnt: Atomic::new(0),
            last_alarm_cnt: Atomic::new(0),
            status: Atomic::new(TimerFdStatus::STOP),
            pollee: Pollee::new(Events::empty()),
        })
    }

    pub fn set_time(&self, flags: TimerSetFlags, new_value: &itimerspec_t) -> Result<itimerspec_t> {
        let mut old_value = itimerspec_t::default();
        let cur_time =
            timespec_t::from(vdso_time::clock_gettime(self.clockid).unwrap()).as_duration();

        let fire_time = match flags {
            TimerSetFlags::TFD_TIMER_ABSTIME => new_value.it_value.as_duration(),
            _ => cur_time
                .checked_add(new_value.it_value.as_duration())
                .unwrap(),
        };

        old_value.it_value = timespec_t::from(self.fire_time.load(Ordering::Relaxed));
        old_value.it_interval = timespec_t::from(self.cur_interval.load(Ordering::Relaxed));
        self.cur_interval
            .store(new_value.it_interval.as_duration(), Ordering::Relaxed);

        if fire_time.is_zero() == true {
            debug!("TimerFd: stop timer");
            self.status.store(TimerFdStatus::STOP, Ordering::Relaxed);
        } else {
            debug!("TimerFd: start timer");

            self.fire_time.store(fire_time, Ordering::Relaxed);

            if new_value.it_interval.as_duration().is_zero() == true {
                self.status
                    .store(TimerFdStatus::ONE_SHOT, Ordering::Relaxed);
            } else {
                self.status.store(TimerFdStatus::LOOP, Ordering::Relaxed);
            }

            self.alarm_cnt.store(0, Ordering::Relaxed);
            self.last_alarm_cnt.store(0, Ordering::Relaxed);
        }

        Ok(old_value)
    }

    pub fn time(&self) -> Result<itimerspec_t> {
        let mut ret_time = itimerspec_t::default();
        let interval = self.cur_interval.load(Ordering::Relaxed);
        let cur_time =
            timespec_t::from(vdso_time::clock_gettime(self.clockid).unwrap()).as_duration();

        if self.status.load(Ordering::Relaxed) != TimerFdStatus::STOP {
            let delta = match self.fire_time.load(Ordering::Relaxed).checked_sub(cur_time) {
                Some(duration) => duration,
                _ => Duration::new(0, 0),
            };

            if self.status.load(Ordering::Relaxed) == TimerFdStatus::ONE_SHOT {
                if delta.is_zero() == true {
                    self.alarm_cnt.store(1, Ordering::Relaxed);
                    self.status.store(TimerFdStatus::STOP, Ordering::Relaxed);
                }

                ret_time.it_value = timespec_t::from(delta);
            } else {
                // loop mode
                if delta.is_zero() == false {
                    // still in the initial it_value alarm range
                    ret_time.it_value = timespec_t::from(delta);
                } else {
                    // In the loop interval range
                    let delta = match cur_time.checked_sub(self.fire_time.load(Ordering::Relaxed)) {
                        Some(duration) => duration,
                        _ => Duration::new(0, 0),
                    };

                    let div = delta.div_duration_f32(interval).ceil() as u32;
                    let remaining = match interval.checked_mul(div).unwrap().checked_sub(delta) {
                        Some(duration) => duration,
                        _ => Duration::new(0, 0),
                    };

                    self.alarm_cnt.store(div as usize, Ordering::Relaxed);
                    ret_time.it_value = timespec_t::from(remaining);
                }
            }
        }

        ret_time.it_interval = timespec_t::from(interval);
        Ok(ret_time)
    }

    fn get_alarm_cnt(&self) -> (usize, Duration) {
        let time = self.time();
        let alarm_cnt = self.alarm_cnt.load(Ordering::Relaxed);
        let last_cnt = self.last_alarm_cnt.load(Ordering::Relaxed);
        let mut cnt = 0;
        if alarm_cnt > last_cnt {
            cnt = alarm_cnt - last_cnt;
        }

        (cnt, time.unwrap().it_value.as_duration())
    }

    async fn wait_for_alarm(&self) -> Result<usize> {
        let waiter = Waiter::new();
        let mut cnt: usize = 0;
        let mut left = Duration::new(0, 0);

        loop {
            let flags = self.flags.load(Ordering::Relaxed);
            let alarm = self.get_alarm_cnt();
            cnt = alarm.0;
            left = alarm.1;

            if cnt > 0 {
                break;
            } else if flags.contains(TimerCreationFlags::TFD_NONBLOCK) {
                return_errno!(EAGAIN, "try again");
            } else {
                // block read, do wait for left time to rest cpu
                //yield_().await;
                if let Ok(_) = waiter.wait_timeout(Some(&mut left)).await {
                    // TODO: support interrupt sleep.
                    // return_errno!(EINTR, "sleep interrupted");
                    unreachable!("this waiter can not be interrupted");
                }
            }
        }

        Ok(cnt)
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

impl TimerFd {
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        if buf.len() < 8 {
            return_errno!(EINVAL, "buffer is too small");
        }

        let buf = &mut buf[0..8];
        let mut cnt = 0;
        match self.wait_for_alarm().await {
            Ok(size) => cnt = size,
            Err(_) => return_errno!(EAGAIN, "try again"),
        }

        // Update alarm count
        self.last_alarm_cnt
            .store(self.alarm_cnt.load(Ordering::Relaxed), Ordering::Relaxed);

        let bytes = cnt.to_ne_bytes();
        buf.copy_from_slice(&bytes);

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
        let (count, left) = self.get_alarm_cnt();
        if count > 0 {
            Events::IN
        } else {
            self.pollee.del_events(Events::IN);
            //let (tx, rx): (Sender<(Pollee, Duration)>, Receiver<(Pollee, Duration)>)= mpsc::channel();
            let (tx, rx): (Sender<(Pollee, Duration)>, Receiver<(Pollee, Duration)>) =
                flume::unbounded();
            async_rt::task::SpawnOptions::new(async move {
                let waiter = Waiter::new();
                let (pollee, mut left) = rx.recv().unwrap();

                if let Ok(_) = waiter.wait_timeout(Some(&mut left)).await {
                    // TODO: support interrupt sleep.
                    // return_errno!(EINTR, "sleep interrupted");
                    unreachable!("this waiter can not be interrupted");
                }

                pollee.add_events(Events::IN)
            })
            .priority(async_rt::sched::SchedPriority::Low)
            .spawn();

            tx.send((self.pollee.clone(), left)).unwrap();

            self.pollee.poll(mask, poller)
        }
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
