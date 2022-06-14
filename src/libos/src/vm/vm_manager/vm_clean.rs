use super::*;
use async_rt::task::TaskId;
use async_rt::wait::{Waiter, WaiterQueue};
use flume::{unbounded, Receiver, Sender};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicPtr, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;
use vm_chunk_manager::ChunkManager;

/// Due to the lack of the page table, for applications running inside Occlum, when the user mmap a range of memory,
/// the pages corresponding to the memory are actually committed and require clean-up even if the user never uses the memory.
/// For some applications, it will try to mmap a big range of memory, do nothing on it and just unmap the memory. In this case,
/// the clean-up process will consume a lot of time and can potentially cause the application to misbehave. This design
/// is trying to reduce the time of munmap to minimize this gap.
///
/// 1. Define the clean workers
/// There are two kinds of workers: clean workers and help workers.
/// The clean workers are generated at the init stage of the user space VM. And their only works are to clean the dirty range after munmap.
/// The help workers are actually application threads which are trying to allocate memory but failed due to insufficient memory. Thus, the
/// application thread will become a help worker to help cleaning the dirty range. And once the request queue is empty, it will become application
/// thread again.
///
/// 2. Define the clean request.
/// We use `CleanReq` to define the cleaning process of a range. A clean request can be generated when munmap a range.
/// And then the request is responded by the clean worker to do the actual cleaning (i.e. set 0). And when the cleaning is done, the clean
/// worker will put the range back to the free space manager for future allocation.
///
/// 3. Define the structure to handle the clean request
/// There could be multiple threads from multiple processes munmap-ing different ranges. And there can be clean_worker_max_num of threads
/// doing the cleaning. Thus, use MPMC queue provided by flume.
///
/// 4. Define the rule for the clean request
/// If clean request < `clean_req_threashold`, clean by the requesting thread itself. Only if the clean request is greater than `clean_req_threashold`,
/// send to the clean queue to let clean workers do the cleaning. Because cleaning small range is fast, and sending to other threads will introduce
/// more overhead to the entire system.
/// If clean_req_threashold < request < req_max_size, the clean thread will do the cleaning as a single request (use one single thread to do the cleaning)
/// If req_max_size < request, split the request into multiple small requests of req_max_size and make several cleaning threads to do the cleaning.
/// If there are too many clean requests in the queue and the request number exceeds the high watermark, and if the request size is not greater than
/// req_max_size * clean_worker_reject_req_num_threashold, clean by the requesting thread itself. If the request size is greater than that, although
/// the high water mark is exceeded, enqueue the request for future handling.
///
/// 5. Define the behavior of the clean threads
/// At the init stage, create clean_worker_max_num threads. And if a clean worker thread can't receive requests, it will wait. Once a request is
/// sent by requesting thread, it will also wake one worker. The clean workers should be run with low priority. If there are many user tasks pending,
/// the clean worker shouldn't clean anymore.
///
/// 6. What if the free space is not enough
/// For mmap, if the desired memory is not available while there are many requests in the queue, the allocation thread should become a help worker
/// and help respond to the requests in the queue until the queue is empty. And then it will try to allocate for the second time. And if it still fails,
/// it becomes the help worker again but this time, check and wait for other workers' status until the cleaning is done. Then it will try to allocate
/// for the third time. If this time it still fails, return with the error number.

lazy_static! {
    pub(super) static ref CLEAN_QUEUE: CleanQueue = CleanQueue::init();
}

// This function is called to init all clean workers.
pub(super) fn init_vm_clean_workers() -> Result<()> {
    let clean_worker_num = CLEAN_QUEUE.config().clean_worker_max_num;
    (0..clean_worker_num).into_iter().for_each(|_| {
        async_rt::task::spawn(async {
            clean_worker_main_func().await;
        });
    });
    Ok(())
}

// This function is called by non-cleaning thread to help clean the dirty range when the user thread fails to allocate memory
pub(super) fn become_help_worker() {
    // When this function is called, the memory resources are not suffient. If there are many cleaning requests, all the clean
    // workers should be busy, and together with this help worker, the clean queue will soon be empty.
    CLEAN_QUEUE.help_worker_recv_req_until_empty();
}

pub struct CleanQueue {
    sender: Sender<CleanReq>,
    receiver: Receiver<CleanReq>,
    waiter_queue: WaiterQueue,
    // Worker status maps are to record the ranges being cleaned. Seperate to clean workers and help workers here because the clean worker status map
    // will keep the number of elements but only update the current request. But Help worker map will need to update the elements when a normal thread
    // becomes a help worker.
    clean_worker_status_map: RwLock<HashMap<TaskId, CurrentReq>>,
    help_worker_status_map: RwLock<HashSet<VMRange>>,
    // Some values for configuring this struct
    config: CleanConfig,
}

impl CleanQueue {
    fn init() -> Self {
        let mpmc = flume::unbounded();
        let waiter_queue = WaiterQueue::new();
        let config = CleanConfig::default();
        let clean_worker_status_map =
            RwLock::new(HashMap::with_capacity(config.clean_worker_max_num));
        let help_worker_init_cap = 4;
        let help_worker_status_map = RwLock::new(HashSet::with_capacity(help_worker_init_cap));
        Self {
            sender: mpmc.0,
            receiver: mpmc.1,
            waiter_queue,
            clean_worker_status_map,
            help_worker_status_map,
            config,
        }
    }

    pub(super) fn send_reqs(&self, mut reqs: Vec<CleanReq>) -> Result<()> {
        if self.is_over_high_watermark()
            && reqs.len() <= self.config.clean_worker_reject_req_num_threashold
        {
            // There are too many cleaning requests in the queue, and since the new requests are not too big,
            // return and let the current thread do the cleaning.
            return_errno!(ENOMEM, "there are too many cleaning requests");
        }

        reqs.into_iter().for_each(|req| {
            CLEAN_QUEUE.sender().send(req).expect("send request error");
            CLEAN_QUEUE.waiter_queue().wake_one();
        });

        Ok(())
    }

    pub(super) fn is_clean_worker_needed(&self, clean_size: usize) -> bool {
        clean_size > self.config.clean_req_threashold
    }

    pub(super) fn has_potential_free_memory(&self) -> bool {
        if self.sender.is_empty() {
            return false;
        }

        return true;
    }

    // This function can be called when the enclave exits. There is no need to clean the memory anymore. Just return all the memory
    // back to pass the integrity check of memory at the end.
    pub(super) fn return_back_ranges_without_cleaning(&self) {
        while let Ok(req) = self.receiver.try_recv() {
            &req.return_back_range(&req.range)
                .expect("return back range error");
        }
    }

    fn sender(&self) -> &Sender<CleanReq> {
        &self.sender
    }

    fn receiver(&self) -> &Receiver<CleanReq> {
        &self.receiver
    }

    fn waiter_queue(&self) -> &WaiterQueue {
        &self.waiter_queue
    }

    fn clean_worker_status_map(&self) -> &RwLock<HashMap<TaskId, CurrentReq>> {
        &self.clean_worker_status_map
    }

    fn help_worker_status_map(&self) -> &RwLock<HashSet<VMRange>> {
        &self.help_worker_status_map
    }

    fn config(&self) -> &CleanConfig {
        &self.config
    }

    fn is_over_high_watermark(&self) -> bool {
        self.receiver.len() > self.config().clean_que_high_watermark
    }

    async fn init_clean_worker(&self, waiter: &mut Waiter) -> Result<TaskId> {
        self.waiter_queue.enqueue(waiter);
        let task_id = current_task_id();
        self.clean_worker_status_map
            .write()
            .unwrap()
            .insert(task_id, CurrentReq::default());
        waiter.wait().await;
        Ok(task_id)
    }

    async fn clean_worker_wait_for_req(waiter: &Waiter) {
        waiter.reset();
        waiter.wait().await;
    }

    fn clean_worker_recv_req(&self, clean_task: &TaskId) -> bool {
        // It is OK to hold the lock of clean_worker_status_map because no other clean workers will need write lock anymore.
        // All clean worker can easily get read lock.
        let clean_worker_status = self.clean_worker_status_map.read().unwrap();
        let current_req = clean_worker_status.get(&clean_task).unwrap();
        if let Ok(req) = self.receiver.try_recv() {
            let dirty_range = req.range;
            current_req.set_current_cleaning_range(dirty_range);
            &req.clean_dirty_range_and_return_back()
                .expect("clean and return dirty range error");
            current_req.done_cleaning_range();
            true
        } else {
            false
        }
    }

    fn help_worker_recv_req_until_empty(&self) {
        while let Ok(req) = self.receiver.try_recv() {
            let dirty_range = req.range;
            self.help_worker_status_map
                .write()
                .unwrap()
                .insert(dirty_range);
            &req.clean_dirty_range_and_return_back()
                .expect("clean and return dirty range error");
            self.help_worker_status_map
                .write()
                .unwrap()
                .remove(&dirty_range);
        }
    }

    // This function can be called when mmap with forced or hint address and before the real allocation.
    pub async fn clean_and_wait_for_range_if_any(&self, target_range: &VMRange) {
        // Limitation: Due to the limitation of MPMC channel, we can only know the request content after
        // received, so we can't iterate the clean queue to find all the overlap requests. Thus we must empty
        // the clean queue first.
        self.help_worker_recv_req_until_empty();

        // There is no dirty ranges in the target range.
        // Wait for overlaped ranges being cleaned to finish

        loop {
            if self
                .wait_for_clean_worker_overlaped_range(target_range)
                .await
                && self
                    .wait_for_help_worker_overlaped_range(target_range)
                    .await
            {
                break;
            }
        }
    }

    async fn wait_for_clean_worker_overlaped_range(&self, target_range: &VMRange) -> bool {
        let mut done = false;
        let clean_worker_status = self.clean_worker_status_map.read().unwrap();
        if clean_worker_status
            .iter()
            .find(|(_, req)| req.overlap_with(&target_range))
            .is_some()
        {
            drop(clean_worker_status);
            // Since big requests are split into small ones, the clean process shouldn't be long. Just yield here
            // and loop again.
            async_rt::sched::yield_().await;
        } else {
            done = true;
        }

        done
    }

    async fn wait_for_help_worker_overlaped_range(&self, target_range: &VMRange) -> bool {
        let mut done = false;
        let help_worker_status = self.help_worker_status_map.read().unwrap();
        if help_worker_status
            .iter()
            .find(|range| range.overlap_with(&target_range))
            .is_some()
        {
            drop(help_worker_status);
            async_rt::sched::yield_().await;
        } else {
            done = true;
        }

        done
    }
}

struct CleanConfig {
    clean_worker_max_num: usize,
    clean_req_threashold: usize, // If the dirty range is greater than this, clean workers will be used
    clean_que_high_watermark: usize, // The clean queue is unbounded. If the request number is bigger than this, don't send to the clean queue anymore.
    req_max_size: usize, // If the dirty range is greater than this, split the big request to multiple requests
    // When the clean queue is over the high water mark, and the request number(after splitted) is smaller than this threashold,
    // clean queue will reject the reqeust to let the non-cleaning thread do the work to reduce the pressure of clean workers.
    clean_worker_reject_req_num_threashold: usize,
}

// Since many of the modules of the libos will need performance tuning, including scheduling, locking, etc. Many default values
// here could also need more adjustment for better performance.
// TODO: Adjust default values for better performance.
impl Default for CleanConfig {
    fn default() -> Self {
        let clean_worker_max_num = (async_rt::config::parallelism() as f32 / 4.0).ceil() as usize;
        CleanConfig {
            clean_worker_max_num: clean_worker_max_num,
            clean_req_threashold: 256 * 1024, // 256 KB
            clean_que_high_watermark: 8000 * clean_worker_max_num,
            req_max_size: 16 * 1024 * 1024, // 16 MB
            clean_worker_reject_req_num_threashold: 2,
        }
    }
}

struct CurrentReq(SgxMutex<Option<VMRange>>);

impl Default for CurrentReq {
    fn default() -> Self {
        Self(SgxMutex::new(None))
    }
}

impl CurrentReq {
    fn new(range: VMRange) -> Self {
        Self(SgxMutex::new(Some(range)))
    }

    fn set_current_cleaning_range(&self, range: VMRange) {
        *self.0.lock().unwrap() = Some(range);
    }

    fn done_cleaning_range(&self) {
        *self.0.lock().unwrap() = None;
    }

    fn overlap_with(&self, target_range: &VMRange) -> bool {
        let inner = self.0.lock().unwrap();
        match *inner {
            None => return false,
            Some(range) => {
                return range.overlap_with(target_range);
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Inner {
    Single,
    Multi(Arc<SgxMutex<ChunkManager>>),
}

#[derive(Clone)]
pub struct CleanReq {
    range: VMRange,
    inner: Inner,
}

impl Debug for CleanReq {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("CleanReq")
            .field("range", &self.range)
            .finish()
    }
}

impl CleanReq {
    pub(super) fn new_single_vma_reqs(range: VMRange) -> Vec<Self> {
        if range.size() <= CLEAN_QUEUE.config().req_max_size {
            return vec![Self {
                range,
                inner: Inner::Single,
            }];
        }

        Self::split_req(range, None)
    }

    pub(super) fn new_reqs(
        range: VMRange,
        chunk_manager_ref: Option<Arc<SgxMutex<ChunkManager>>>,
    ) -> Vec<Self> {
        if range.size() <= CLEAN_QUEUE.config().req_max_size {
            return vec![Self {
                range,
                inner: Inner::Multi(chunk_manager_ref.unwrap()),
            }];
        }

        Self::split_req(range, chunk_manager_ref)
    }

    fn clean_dirty_range_and_return_back(&self) -> Result<()> {
        let dirty_range = &self.range;
        // Reset to zero
        unsafe {
            dirty_range.clean();
        }

        return self.return_back_range(dirty_range);
    }

    fn inner(&self) -> &Inner {
        &self.inner
    }

    fn split_req(range: VMRange, chunk_manager: Option<Arc<SgxMutex<ChunkManager>>>) -> Vec<Self> {
        let inner = if chunk_manager.is_some() {
            Inner::Multi(chunk_manager.unwrap())
        } else {
            Inner::Single
        };

        let req_max_size = CLEAN_QUEUE.config().req_max_size;
        let num = range.size() / req_max_size;
        let reqs = (0..num)
            .into_iter()
            .map(|i| {
                let range_start = range.start() + i * req_max_size;
                let range_end = if i < num - 1 {
                    range_start + req_max_size
                } else {
                    range.end()
                };
                Self {
                    // Just split to small ranges, safe
                    range: unsafe { VMRange::from_unchecked(range_start, range_end) },
                    inner: inner.clone(),
                }
            })
            .collect::<Vec<CleanReq>>();

        reqs
    }

    fn return_back_range(&self, range: &VMRange) -> Result<()> {
        match self.inner() {
            Inner::Multi(chunk) => {
                chunk.lock().unwrap().return_clean_vm(&range)?;
            }
            Inner::Single => {
                // For single vma chunk, after cleaning, the range will be returned to global vm manger.
                USER_SPACE_VM_MANAGER
                    .internal()
                    .free_manager_mut()
                    .add_range_back_to_free_manager(&range)?;
            }
        }
        Ok(())
    }
}

async fn clean_worker_main_func() -> Result<()> {
    let mut waiter = Waiter::new();
    let clean_task = CLEAN_QUEUE.init_clean_worker(&mut waiter).await?;
    loop {
        if async_rt::executor::is_shutdown() {
            CLEAN_QUEUE.return_back_ranges_without_cleaning();
        }

        if CLEAN_QUEUE.clean_worker_recv_req(&clean_task) {
            continue;
        } else {
            CleanQueue::clean_worker_wait_for_req(&waiter).await;
        }
    }

    // Clean workers will safely exit when notified by the executor during waiting.
    unreachable!();
}

fn current_task_id() -> TaskId {
    async_rt::task::current::get().tid()
}
