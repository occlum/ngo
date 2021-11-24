use crate::prelude::*;
use crate::wait::{Waiter, WaiterQueue};

pub struct Signal {
    count: Mutex<i32>,
    waiter_queue: WaiterQueue,
}

impl Signal {
    pub fn new() -> Self {
        Self {
            count: Mutex::new(0),
            waiter_queue: WaiterQueue::new(),
        }
    }
    pub fn produce(&self) {
        let mut count = self.count.lock();
        *count += 1;
        self.waiter_queue.wake_all();
    }

    pub fn empty(&self) -> bool {
        let mut count = self.count.lock();
        *count == 0
    }

    pub fn consume(&self) {
        let mut count = self.count.lock();
        *count -= 1;

        if *count < 0 {
            panic!("signal number is incorrect");
        }
    }

    pub fn waiter(&self) -> Option<Arc<Waiter>> {
        let mut waiter = Waiter::new();

        let count = self.count.lock();
        if *count == 0 {
            self.waiter_queue.enqueue(&mut waiter);
            Some(Arc::new(waiter))
        } else {
            None
        }
    }

    pub fn unregister(&self, waiter: &mut Arc<Waiter>) {
        let count = self.count.lock();
        let mut waiter = Arc::<Waiter>::get_mut(waiter).unwrap();
        self.waiter_queue.dequeue(&mut waiter);
    }
}
