use alloc::sync::Weak;
use core::fmt::{self, Debug};

use futures::task::ArcWake;

use crate::executor::EXECUTOR;
use crate::prelude::*;
use crate::sched::{SchedInfo, SchedPriority};
use crate::task::{LocalsMap, TaskId, Tirqs};

pub struct Task {
    tid: TaskId,
    sched_info: Arc<dyn SchedInfo>,
    future: Mutex<Option<BoxFuture<'static, ()>>>,
    locals: LocalsMap,
    tirqs: Tirqs,
    // Used by executor to avoid a task consuming too much space in enqueues
    // due to a task being enqueued multiple times.
    is_enqueued: AtomicBool,
    weak_self: Weak<Self>,
}

impl Task {
    pub fn tid(&self) -> TaskId {
        self.tid
    }

    pub fn sched_info(&self) -> &dyn SchedInfo {
        self.sched_info.as_ref()
    }

    pub fn tirqs(&self) -> &Tirqs {
        &self.tirqs
    }

    /// Get the task that a given tirqs is associated to.
    ///
    /// # Safety
    ///
    /// This behavior of this function is undefined if the given tirqs is not
    /// a field of a task.
    pub(crate) fn from_tirqs(tirqs: &Tirqs) -> Arc<Self> {
        use intrusive_collections::container_of;

        let tirqs_ptr = tirqs as *const _;
        // Safety. The pointer is valid and the field-container relationship is hold
        let task_ptr = unsafe { container_of!(tirqs_ptr, Task, tirqs) };
        // Safety. The container's pointer is valid as long as the field's pointer is valid.
        let task = unsafe { &*task_ptr };
        task.to_arc()
    }

    pub(crate) fn future(&self) -> &Mutex<Option<BoxFuture<'static, ()>>> {
        &self.future
    }

    pub(crate) fn locals(&self) -> &LocalsMap {
        &self.locals
    }

    pub(crate) fn to_arc(&self) -> Arc<Self> {
        self.weak_self.upgrade().unwrap()
    }

    /// Returns whether the task is enqueued.
    pub(crate) fn is_enqueued(&self) -> bool {
        self.is_enqueued.load(Ordering::Relaxed)
    }

    /// Try to set the status of the task as enqueued.
    ///
    /// If the task is already enqueued, return an error.
    pub(crate) fn try_set_enqueued(&self) -> Result<()> {
        self.is_enqueued
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .map(|_| ())
            .map_err(|_| errno!(EEXIST, "already enqueued"))
    }

    /// Set the status of the task as NOT enqueued.
    pub(crate) fn reset_enqueued(&self) {
        self.is_enqueued.store(false, Ordering::Relaxed);
    }
}

unsafe impl Sync for Task {}

impl Drop for Task {
    fn drop(&mut self) {
        // Drop the locals explicitly so that we can take care of any potential panics
        // here. One possible reason of panic is the drop method of a task-local variable
        // requires accessinng another already-dropped task-local variable.
        // TODO: handle panic
        unsafe {
            self.locals.clear();
        }
    }
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        EXECUTOR.wake_task(arc_self);
    }
}

impl Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task").field("tid", &self.tid).finish()
    }
}

pub struct TaskBuilder {
    future: Option<BoxFuture<'static, ()>>,
    priority: SchedPriority,
}

impl TaskBuilder {
    pub fn new(future: impl Future<Output = ()> + 'static + Send) -> Self {
        Self {
            future: Some(future.boxed()),
            priority: SchedPriority::Normal,
        }
    }

    pub fn priority(mut self, priority: SchedPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn build(&mut self) -> Arc<Task> {
        assert!(self.future.is_some());

        let tid = TaskId::new();
        let sched_info = EXECUTOR.sched_info(self.priority);
        let future = Mutex::new(self.future.take());
        let locals = LocalsMap::new();
        // Safety. The tirqs will be inserted into a Task before using it.
        let tirqs = unsafe { Tirqs::new() };
        let is_enqueued = AtomicBool::new(false);
        let weak_self = Weak::new();
        let task = Task {
            tid,
            sched_info,
            future,
            locals,
            tirqs,
            is_enqueued,
            weak_self,
        };
        // Create an Arc and update the weak_self
        new_self_ref_arc::new_self_ref_arc!(task)
    }
}
