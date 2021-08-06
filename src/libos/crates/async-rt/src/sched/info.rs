use spin::rw_lock::RwLock;

use crate::executor::EXECUTOR;
use crate::prelude::*;
use crate::sched::Affinity;

/// A per-task scheduling-related info.
pub struct SchedInfo {
    last_thread_id: AtomicU32,
    affinity: RwLock<Affinity>,
    priority: SchedPriority,
    enqueue_tick: AtomicU64,
}

impl SchedInfo {
    pub fn new(priority: SchedPriority) -> Self {
        static LAST_THREAD_ID: AtomicU32 = AtomicU32::new(0);

        let last_thread_id = {
            let last_thread_id =
                LAST_THREAD_ID.fetch_add(1, Ordering::Relaxed) % EXECUTOR.parallelism();
            AtomicU32::new(last_thread_id)
        };
        let affinity = RwLock::new(Affinity::new_full());
        let enqueue_tick = AtomicU64::new(0);

        Self {
            last_thread_id,
            affinity,
            priority,
            enqueue_tick,
        }
    }

    #[inline]
    pub fn last_thread_id(&self) -> u32 {
        self.last_thread_id.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_last_thread_id(&self, id: u32) {
        self.last_thread_id.store(id, Ordering::Relaxed);
    }

    #[inline]
    pub fn affinity(&self) -> &RwLock<Affinity> {
        &self.affinity
    }

    #[inline]
    pub fn priority(&self) -> &SchedPriority {
        &self.priority
    }

    #[inline]
    pub fn enqueue_tick(&self) -> u64 {
        self.enqueue_tick.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_enqueue_tick(&self, tick: u64) {
        self.enqueue_tick.store(tick, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SchedPriority {
    High,
    Normal,
    Low,
}
