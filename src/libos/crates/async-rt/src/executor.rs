use flume::{Receiver, Sender, TrySendError};
use futures::task::waker_ref;
use spin::mutex::MutexGuard;

use crate::config::CONFIG;
#[cfg(feature = "thread_sleep")]
use crate::parks::Parks;
use crate::prelude::*;
use crate::sched::{Affinity, SchedPriority};
use crate::task::Task;

pub fn parallelism() -> u32 {
    EXECUTOR.parallelism()
}

pub fn run_tasks() {
    EXECUTOR.run_tasks()
}

pub fn shutdown() {
    EXECUTOR.shutdown()
}

lazy_static! {
    pub(crate) static ref EXECUTOR: Executor = {
        let parallelism = CONFIG.parallelism();
        Executor::new(parallelism).unwrap()
    };
}

pub(crate) struct Executor {
    // The parallelism of the executor.
    parallelism: u32,
    // Each worker corresponds to one thread.
    workers: Vec<Worker>,
    // Global queues shared by all workers.
    injector: Injector,
    // Global increasing ticks, used to calculate latencies.
    ticks: AtomicU64,
    // The task latency for each worker.
    latencies: Vec<AtomicU64>,
    // Used to generate worker id.
    next_worker_id: AtomicU32,
    // Whether the executor is shutdown
    is_shutdown: AtomicBool,
    // The lock for rebalance-workload operation.
    // The value guarded by the lock is the last tick when trigger rebalance.
    rebalance_lock: Mutex<u64>,
    // Used to thread park / unpark
    #[cfg(feature = "thread_sleep")]
    parks: Parks,
}

impl Executor {
    const RE_BALANCE_MOD: u64 = 64;

    pub fn new(parallelism: u32) -> Result<Self> {
        if parallelism == 0 {
            return Err("invalid argument");
        }

        let workers = (0..parallelism).map(|_| Worker::new()).collect();
        let injector = Injector::new();
        let ticks = AtomicU64::new(0);
        let latencies = (0..parallelism).map(|_| AtomicU64::new(0)).collect();
        let next_worker_id = AtomicU32::new(0);
        let is_shutdown = AtomicBool::new(false);
        let rebalance_lock = Mutex::new(0);

        #[cfg(feature = "thread_sleep")]
        let parks = Parks::new(parallelism);

        let new_self = Self {
            parallelism,
            workers,
            injector,
            ticks,
            latencies,
            next_worker_id,
            is_shutdown,
            rebalance_lock,
            #[cfg(feature = "thread_sleep")]
            parks,
        };
        Ok(new_self)
    }

    pub fn parallelism(&self) -> u32 {
        self.parallelism
    }

    pub fn run_tasks(&self) {
        let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed) as usize;
        assert!(worker_id < self.parallelism as usize);
        let worker = &self.workers[worker_id];

        loop {
            let ticks = self.inc_ticks();
            // Try to do rebalance every once in a while.
            if ticks % Executor::RE_BALANCE_MOD == 0 {
                self.try_rebalance_workload();
            }

            let task = {
                let task_option = worker.pop();

                if self.is_shutdown.load(Ordering::Relaxed) {
                    return;
                }

                match task_option {
                    Some(task) => task,
                    None => {
                        #[cfg(feature = "thread_sleep")]
                        self.parks.park_timeout(
                            worker_id as usize,
                            core::time::Duration::from_millis(10),
                        );
                        #[cfg(not(feature = "thread_sleep"))]
                        core::sync::atomic::spin_loop_hint();
                        continue;
                    }
                }
            };

            let latency = self
                .ticks()
                .checked_sub(task.sched_info().enqueue_tick())
                .unwrap_or(0);
            self.latencies[worker_id].store(latency, Ordering::Relaxed);

            let mut future_slot = task.future().lock();
            let mut future = match future_slot.take() {
                None => continue,
                Some(future) => future,
            };
            drop(future_slot);

            crate::task::current::set(task.clone());

            let waker = waker_ref(&task);
            let context = &mut Context::from_waker(&*waker);
            if let Poll::Pending = future.as_mut().poll(context) {
                let mut future_slot = task.future().lock();
                *future_slot = Some(future);
            }

            crate::task::current::reset();
        }
    }

    pub fn accept_task(&self, task: Arc<Task>) {
        if self.is_shutdown() {
            return;
        }

        let thread_id = self.pick_thread_for(&task);
        task.sched_info().set_enqueue_tick(self.ticks());
        self.workers[thread_id].push(task, &self.injector);

        #[cfg(feature = "thread_sleep")]
        self.parks.unpark(thread_id);
    }

    fn pick_thread_for(&self, task: &Arc<Task>) -> usize {
        let last_thread_id = task.sched_info().last_thread_id() as usize;
        let priority = task.sched_info().priority();
        let candidates_num = core::cmp::max((self.parallelism / 2) as usize, 1);
        let latency_weights: Vec<f64> = {
            let tmp_latencies: Vec<u64> = (0..self.parallelism as usize)
                .map(|idx| self.latencies[idx].load(Ordering::Relaxed))
                .collect();
            let max_latency = core::cmp::max(*tmp_latencies.iter().max().unwrap(), 1);
            let min_latency = *tmp_latencies.iter().min().unwrap();

            tmp_latencies
                .iter()
                .map(|l| (l - min_latency) as f64 / max_latency as f64)
                .collect()
        };

        let affinity = task.sched_info().affinity().read();
        assert!(!affinity.is_empty());
        let thread_id = affinity
            .iter()
            .enumerate()
            .chain(affinity.iter().enumerate())
            .skip(last_thread_id)
            .filter(|(_idx, bit)| *bit)
            .take(candidates_num)
            .map(move |(idx, _bit)| {
                let len = self.workers[idx].relax_len(*priority);
                if len == Worker::QUEUED_TASKS_MAX_SIZE {
                    return (idx, 0);
                }
                let capacity_weight = 1.0 - len as f64 / Worker::QUEUED_TASKS_MAX_SIZE as f64; // 0.0 ~ 1.0
                let affinity_weight = (idx == last_thread_id) as u64 as f64; // 0.0 or 1.0
                let weight = capacity_weight * 700.0
                    + latency_weights[idx] * 200.0
                    + affinity_weight * 100.0;
                (idx, weight as u64)
            })
            .max_by_key(|x| x.1)
            .unwrap()
            .0;
        drop(affinity);

        task.sched_info().set_last_thread_id(thread_id as u32);
        thread_id
    }

    fn try_rebalance_workload(&self) {
        if let Some(mut guard) = self.rebalance_lock.try_lock() {
            let ticks = self.ticks();
            if ticks.checked_sub(*guard).unwrap_or(0) < Self::RE_BALANCE_MOD {
                return;
            }
            *guard = ticks;

            self.do_part_rebalance(&guard, SchedPriority::High);
            self.do_part_rebalance(&guard, SchedPriority::Normal);
            self.do_part_rebalance(&guard, SchedPriority::Low);

            #[cfg(feature = "thread_sleep")]
            self.workers
                .iter()
                .enumerate()
                .for_each(|(idx, _worker)| self.parks.unpark(idx));
        }
    }

    fn do_part_rebalance(&self, _guard: &MutexGuard<u64>, priority: SchedPriority) {
        let mut worker_lens: Vec<usize> = (0..self.parallelism as usize)
            .map(|idx| self.workers[idx].relax_len(priority))
            .collect();

        if self.injector.len(priority) > 0 {
            // Try to move tasks from injector to workers.
            while let Some(task) = self.injector.pop_with_priority(priority) {
                let affinity = task.sched_info().affinity().read();
                assert!(!affinity.is_empty());
                // Find the worker with the shortest queue.
                let idx = affinity
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, bit)| if bit { Some(idx) } else { None })
                    .min_by_key(|k| worker_lens[*k])
                    .unwrap();
                drop(affinity);

                // Refresh the enqueue_tick.
                task.sched_info().set_enqueue_tick(self.ticks());
                // Try to push task to the worker.
                if !self.workers[idx].push(task, &self.injector) {
                    // The worker's queue is full. We think workers have heavy workloads, stop moving tasks from injector.
                    break;
                }
                // Update worker queue length
                worker_lens[idx] += 1;
            }

            // Refresh worker queue length
            for idx in 0..worker_lens.len() {
                worker_lens[idx] = self.workers[idx].relax_len(priority);
            }
        }

        let avg_len = worker_lens.iter().sum::<usize>() / self.parallelism as usize;
        let heavy_limit = (Worker::QUEUED_TASKS_MAX_SIZE as f64 * 0.9) as usize;
        let target_len = core::cmp::max(core::cmp::min(avg_len, heavy_limit), 2);
        let mut sorted_lens: Vec<(usize, usize)> = worker_lens
            .iter()
            .enumerate()
            .map(|(idx, len)| (idx, *len))
            .collect();
        // Sort from small to large
        sorted_lens.sort_unstable_by_key(|k| k.1);
        let (mut left, mut right) = (0, self.parallelism as usize - 1);

        // Try to move tasks from heavy workers (right side) to light workers (left side).
        while left < right {
            let (src_idx, src_len) = (sorted_lens[right].0, sorted_lens[right].1);
            if src_len <= target_len {
                right -= 1;
                continue;
            }

            let (dst_idx, dst_len) = (sorted_lens[left].0, sorted_lens[left].1);
            if dst_len >= target_len {
                left += 1;
                continue;
            }

            let check_func = |taskref: &Arc<Task>| {
                let affinity = taskref.sched_info().affinity().read();
                if affinity.is_full() {
                    return Some(dst_idx);
                }
                assert!(!affinity.is_empty());
                let target_idx = affinity
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, bit)| if bit { Some(idx) } else { None })
                    .min_by_key(|k| worker_lens[*k])
                    .unwrap();
                drop(affinity);
                if worker_lens[target_idx] < target_len {
                    Some(target_idx)
                } else {
                    None
                }
            };

            if let Some((target_idx, task)) =
                self.workers[src_idx].checked_pop_with_priority(check_func, priority)
            {
                let sorted_idx = sorted_lens
                    .iter()
                    .filter(|(idx, _len)| *idx == target_idx)
                    .nth(0)
                    .unwrap()
                    .0;
                // Try to push task to the worker.
                if !self.workers[target_idx].push(task, &self.injector) {
                    // The worker's queue is full. Update the length.
                    worker_lens[target_idx] = Worker::QUEUED_TASKS_MAX_SIZE;
                    sorted_lens[sorted_idx].1 = Worker::QUEUED_TASKS_MAX_SIZE;
                } else {
                    worker_lens[target_idx] += 1;
                    sorted_lens[sorted_idx].1 += 1;
                }
                worker_lens[src_idx] -= 1;
            } else {
                right -= 1;
            }
        }
    }

    #[inline]
    fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    #[inline]
    fn inc_ticks(&self) -> u64 {
        self.ticks.fetch_add(1, Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        self.is_shutdown.store(true, Ordering::Relaxed);

        #[cfg(feature = "thread_sleep")]
        self.parks.unpark_all();
    }

    pub fn is_shutdown(&self) -> bool {
        self.is_shutdown.load(Ordering::Relaxed)
    }
}

pub(crate) struct Worker {
    high_pri_queue: TaskQueueWithSlot,
    normal_pri_queue: TaskQueueWithSlot,
    low_pri_queue: TaskQueueWithSlot,
}

impl Worker {
    pub const QUEUED_TASKS_MAX_SIZE: usize = 1_000;

    pub fn new() -> Self {
        Self {
            high_pri_queue: TaskQueueWithSlot::new(Some(Self::QUEUED_TASKS_MAX_SIZE)),
            normal_pri_queue: TaskQueueWithSlot::new(Some(Self::QUEUED_TASKS_MAX_SIZE)),
            low_pri_queue: TaskQueueWithSlot::new(Some(Self::QUEUED_TASKS_MAX_SIZE)),
        }
    }

    pub fn push(&self, task: Arc<Task>, injector: &Injector) -> bool {
        match task.sched_info().priority() {
            SchedPriority::High => {
                if let Err(t) = self.high_pri_queue.push(task) {
                    injector.push(t);
                    return false;
                }
            }
            SchedPriority::Normal => {
                if let Err(t) = self.normal_pri_queue.push(task) {
                    injector.push(t);
                    return false;
                }
            }
            SchedPriority::Low => {
                if let Err(t) = self.low_pri_queue.push(task) {
                    injector.push(t);
                    return false;
                }
            }
        }
        true
    }

    pub fn pop(&self) -> Option<Arc<Task>> {
        if let Some(task) = self.high_pri_queue.pop() {
            return Some(task);
        }

        if let Some(task) = self.normal_pri_queue.pop() {
            return Some(task);
        }

        if let Some(task) = self.low_pri_queue.pop() {
            return Some(task);
        }

        None
    }

    pub fn pop_with_priority(&self, priority: SchedPriority) -> Option<Arc<Task>> {
        match priority {
            SchedPriority::High => self.high_pri_queue.pop(),
            SchedPriority::Normal => self.normal_pri_queue.pop(),
            SchedPriority::Low => self.low_pri_queue.pop(),
        }
    }

    pub fn checked_pop_with_priority<F>(
        &self,
        f: F,
        priority: SchedPriority,
    ) -> Option<(usize, Arc<Task>)>
    where
        F: FnOnce(&Arc<Task>) -> Option<usize>,
    {
        match priority {
            SchedPriority::High => self.high_pri_queue.checked_pop(f),
            SchedPriority::Normal => self.normal_pri_queue.checked_pop(f),
            SchedPriority::Low => self.low_pri_queue.checked_pop(f),
        }
    }

    // pub fn len(&self, priority: SchedPriority) -> usize {
    //     match priority {
    //         SchedPriority::High => self.high_pri_queue.len(),
    //         SchedPriority::Normal => self.normal_pri_queue.len(),
    //         SchedPriority::Low => self.low_pri_queue.len(),
    //     }
    // }

    pub fn relax_len(&self, priority: SchedPriority) -> usize {
        match priority {
            SchedPriority::High => self.high_pri_queue.relax_len(),
            SchedPriority::Normal => self.normal_pri_queue.relax_len(),
            SchedPriority::Low => self.low_pri_queue.relax_len(),
        }
    }

    pub fn is_empty(&self, priority: SchedPriority) -> bool {
        match priority {
            SchedPriority::High => self.high_pri_queue.is_empty(),
            SchedPriority::Normal => self.normal_pri_queue.is_empty(),
            SchedPriority::Low => self.low_pri_queue.is_empty(),
        }
    }
}

pub(crate) struct Injector {
    high_pri_queue: TaskQueue,
    normal_pri_queue: TaskQueue,
    low_pri_queue: TaskQueue,
}

impl Injector {
    pub fn new() -> Self {
        Self {
            high_pri_queue: TaskQueue::new(None),
            normal_pri_queue: TaskQueue::new(None),
            low_pri_queue: TaskQueue::new(None),
        }
    }

    pub fn push(&self, task: Arc<Task>) {
        match task.sched_info().priority() {
            SchedPriority::High => self.high_pri_queue.push(task).unwrap(),
            SchedPriority::Normal => self.normal_pri_queue.push(task).unwrap(),
            SchedPriority::Low => self.low_pri_queue.push(task).unwrap(),
        };
    }

    pub fn pop(&self) -> Option<Arc<Task>> {
        if let Some(task) = self.high_pri_queue.pop() {
            return Some(task);
        }

        if let Some(task) = self.normal_pri_queue.pop() {
            return Some(task);
        }

        if let Some(task) = self.low_pri_queue.pop() {
            return Some(task);
        }

        None
    }

    pub fn pop_with_priority(&self, priority: SchedPriority) -> Option<Arc<Task>> {
        match priority {
            SchedPriority::High => self.high_pri_queue.pop(),
            SchedPriority::Normal => self.normal_pri_queue.pop(),
            SchedPriority::Low => self.low_pri_queue.pop(),
        }
    }

    pub fn len(&self, priority: SchedPriority) -> usize {
        match priority {
            SchedPriority::High => self.high_pri_queue.len(),
            SchedPriority::Normal => self.normal_pri_queue.len(),
            SchedPriority::Low => self.low_pri_queue.len(),
        }
    }

    pub fn is_empty(&self, priority: SchedPriority) -> bool {
        match priority {
            SchedPriority::High => self.high_pri_queue.is_empty(),
            SchedPriority::Normal => self.normal_pri_queue.is_empty(),
            SchedPriority::Low => self.low_pri_queue.is_empty(),
        }
    }
}

pub(crate) struct TaskQueueWithSlot {
    slot: Mutex<Option<Arc<Task>>>,
    queue: TaskQueue,
}

impl TaskQueueWithSlot {
    pub fn new(capacity: Option<usize>) -> Self {
        Self {
            slot: Mutex::new(None),
            queue: TaskQueue::new(capacity),
        }
    }

    pub fn push(&self, task: Arc<Task>) -> core::result::Result<(), Arc<Task>> {
        self.queue.push(task)
    }

    pub fn pop(&self) -> Option<Arc<Task>> {
        // TODO: maybe use try_lock() instead of lock(), hence we can get a fast path.
        let mut guard = self.slot.lock();
        if guard.is_some() {
            return guard.take();
        }
        drop(guard);

        self.queue.pop()
    }

    pub fn checked_pop<F>(&self, f: F) -> Option<(usize, Arc<Task>)>
    where
        F: FnOnce(&Arc<Task>) -> Option<usize>,
    {
        let mut guard = self.slot.lock();
        if guard.is_none() {
            if let Some(task) = self.queue.pop() {
                *guard = Some(task);
            } else {
                return None;
            }
        }

        f(guard.as_ref().unwrap()).map_or(None, |idx| Some((idx, guard.take().unwrap())))
    }

    #[inline]
    pub fn capacity(&self) -> Option<usize> {
        self.queue.capacity()
    }

    #[inline]
    pub fn len(&self) -> usize {
        let guard = self.slot.lock();
        self.queue.len() + guard.is_some() as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        if self.queue.is_empty() {
            let guard = self.slot.lock();
            return guard.is_none();
        }
        false
    }

    #[inline]
    pub fn relax_len(&self) -> usize {
        self.queue.len()
    }
}

pub(crate) struct TaskQueue {
    recv: Receiver<Arc<Task>>,
    send: Sender<Arc<Task>>,
    capacity: Option<usize>,
}

impl TaskQueue {
    pub fn new(capacity: Option<usize>) -> Self {
        let (send, recv) = match capacity {
            Some(size) => flume::bounded(size),
            None => flume::unbounded(),
        };

        Self {
            recv,
            send,
            capacity,
        }
    }

    pub fn push(&self, task: Arc<Task>) -> core::result::Result<(), Arc<Task>> {
        if self.capacity.is_some() {
            match self.send.try_send(task) {
                Ok(_) => Ok(()),
                Err(e) => match e {
                    TrySendError::Full(t) => Err(t),
                    TrySendError::Disconnected(_) => {
                        panic!("the channel of flume is disconnected.")
                    }
                },
            }
        } else {
            self.send.send(task).unwrap();
            Ok(())
        }
    }

    pub fn pop(&self) -> Option<Arc<Task>> {
        if let Ok(task) = self.recv.try_recv() {
            return Some(task);
        }
        return None;
    }

    #[inline]
    pub fn capacity(&self) -> Option<usize> {
        self.capacity
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.recv.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.recv.is_empty()
    }
}
