use spin::rw_lock::RwLock;

use crate::parks::Parks;
use crate::prelude::*;
use crate::sched::{Affinity, SchedInfo, SchedInfoCommon, SchedPriority};
use crate::task::Task;

use super::{Scheduler, MAX_QUEUED_TASKS};

use flume::{Receiver, Sender};

const DEFAULT_BUDGET: u8 = 64;

pub struct BasicScheduler {
    parallelism: usize,
    run_queues: Vec<Receiver<Arc<Task>>>,
    task_senders: Vec<Sender<Arc<Task>>>,
    parks: Arc<Parks>,
}

impl BasicScheduler {
    pub fn new(parks: Arc<Parks>) -> Self {
        let parallelism = parks.len();
        let mut run_queues = Vec::with_capacity(parallelism);
        let mut task_senders = Vec::with_capacity(parallelism);
        for _ in 0..parallelism {
            let (task_sender, run_queue) = flume::bounded(MAX_QUEUED_TASKS);
            run_queues.push(run_queue);
            task_senders.push(task_sender);
        }

        Self {
            parallelism,
            run_queues,
            task_senders,
            parks,
        }
    }
}

impl Scheduler for BasicScheduler {
    fn enqueue_task(&self, task: Arc<Task>) {
        let affinity = task.sched_info().affinity().read();
        assert!(!affinity.is_empty());
        let mut thread_id = task.sched_info().last_thread_id() as usize;
        while !affinity.get(thread_id) {
            thread_id = (thread_id + 1) % Affinity::max_threads();
        }
        drop(affinity);

        task.sched_info().set_last_thread_id(thread_id as u32);
        self.task_senders[thread_id]
            .send(task)
            .expect("too many tasks enqueued");

        self.parks.unpark(thread_id);
    }

    fn dequeue_task(&self, thread_id: usize) -> Option<Arc<Task>> {
        self.run_queues[thread_id].try_recv().ok()
    }

    fn sched_info(&self, priority: SchedPriority) -> Arc<dyn SchedInfo> {
        Arc::new(BasicSchedInfo::new(priority))
    }

    fn update_budget(&self, schedule_info: &dyn SchedInfo) -> bool {
        schedule_info.consume_budget();
        if !schedule_info.has_remained_budget() {
            schedule_info.reset_budget();
            true
        } else {
            false
        }
    }
}

pub struct BasicSchedInfo {
    schedinfo_common: SchedInfoCommon,
    consumed_budget: AtomicU8,
}

impl BasicSchedInfo {
    pub fn new(priority: SchedPriority) -> Self {
        let schedinfo_common = SchedInfoCommon::new(priority);
        let consumed_budget = AtomicU8::new(0);
        Self {
            schedinfo_common,
            consumed_budget,
        }
    }
}

impl SchedInfo for BasicSchedInfo {
    fn affinity(&self) -> &RwLock<Affinity> {
        &self.schedinfo_common.affinity()
    }

    fn priority(&self) -> SchedPriority {
        self.schedinfo_common.priority()
    }

    fn set_priority(&self, priority: SchedPriority) {
        self.schedinfo_common.set_priority(priority)
    }

    fn last_thread_id(&self) -> u32 {
        self.schedinfo_common.last_thread_id()
    }

    fn set_last_thread_id(&self, id: u32) {
        self.schedinfo_common.set_last_thread_id(id)
    }

    #[cfg(feature = "use_latency")]
    fn enqueue_epochs(&self) -> u64 {
        self.schedinfo_common.enqueue_epochs()
    }

    #[cfg(feature = "use_latency")]
    fn set_enqueue_epochs(&self, data: u64) {
        self.schedinfo_common.set_enqueue_epochs(data)
    }

    fn has_remained_budget(&self) -> bool {
        self.consumed_budget.load(Ordering::Relaxed) < DEFAULT_BUDGET
    }

    fn reset_budget(&self) {
        self.consumed_budget.store(0, Ordering::Relaxed);
    }

    fn consume_budget(&self) {
        self.consumed_budget.fetch_add(1, Ordering::Relaxed);
    }
}
