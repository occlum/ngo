// Implements free space management for memory.
// Currently only use simple vector as the base structure.
//
// The elements in the vector are sorted from (more recently used, smaller ranges) to (least recently used, bigger ranges)
//
// Thus for finding free ranges, the function could return on first range with enough size.
//
// When returing free ranges, it can be inserted just as the first element of free range vector and would be picked first if
// the size is enough. This should be done in the cleaning thread.
//
// TODO: Define strategies for allocation.
// (1) Fast: more recently used ranges will be allocated first. Reduce the number of sorting (no sort when alloc or free), and gain more performance from cache.
//     Only sort when there is not enough size for allocation.
// (2) Effecient: smaller ranges will be allocated first. Sort everytime when there is alloc or free.
// (3) Mixed: Current implementation. Only sort after allocation to put smaller ranges to the front. No sort when free.

use super::vm_manager::VMMapAddr;
use super::*;

static INITIAL_SIZE: usize = 100;

#[derive(Debug, Default)]
pub struct VMFreeSpaceManager {
    free_manager: SgxMutex<Vec<VMRange>>, // sort as (previously used, small range) -> (not used, big range)
}

impl VMFreeSpaceManager {
    pub fn new(initial_free_range: VMRange) -> Self {
        let mut free_manager = Vec::with_capacity(INITIAL_SIZE);
        free_manager.push(initial_free_range);

        VMFreeSpaceManager {
            free_manager: SgxMutex::new(free_manager),
        }
    }

    pub fn inner(&self) -> SgxMutexGuard<Vec<VMRange>> {
        self.free_manager.lock().unwrap()
    }

    pub fn find_free_range_internal(&self, size: usize, addr: VMMapAddr) -> Result<VMRange> {
        // Record the minimal free range that satisfies the contraints
        let mut result_free_range: Option<VMRange> = None;
        let mut result_idx: Option<usize> = None;
        let mut free_list = self.free_manager.lock().unwrap();

        for (idx, free_range) in free_list.iter().enumerate() {
            let mut free_range = {
                if free_range.size() < size {
                    continue;
                }
                // return the whole free range
                unsafe { VMRange::from_unchecked(free_range.start(), free_range.end()) }
            };

            match addr {
                // Want a minimal free_range
                VMMapAddr::Any => {}
                // Prefer to have free_range.start == addr
                VMMapAddr::Hint(addr) => {
                    // if free_range.contains(addr) {
                    //     if free_range.end() - addr >= size {
                    //         free_range.start = addr;
                    //         free_range.end = addr + size;
                    //         Self::free_list_update_range(free_list, idx, free_range);
                    //         return Ok(free_range);
                    //     }
                    // }
                }
                // Must have free_range.start == addr
                VMMapAddr::Need(addr) | VMMapAddr::Force(addr) => {
                    //     if free_range.start() > addr {
                    //         //unsafe {self.spin_lock.0.unlock();}
                    //         return_errno!(ENOMEM, "not enough memory for fixed mmap");
                    //     }
                    //     if !free_range.contains(addr) {
                    //         continue;
                    //     }
                    //     if free_range.end() - addr < size {
                    //         //unsafe {self.spin_lock.0.unlock();}
                    //         return_errno!(ENOMEM, "not enough memory for fixed mmap");
                    //     }
                    //     free_range.start = addr;
                    //     free_range.end = addr + size;
                    //     Self::free_list_update_range(free_list, idx, free_range);
                    //     return Ok(free_range);
                }
            }

            result_free_range = Some(free_range);
            result_idx = Some(idx);
            break;
        }

        // There is not enough free range. We just return here but caller can do a merge and
        // sort and try finding again.
        if result_free_range.is_none() {
            return_errno!(ENOMEM, "not enough memory");
        }

        let index = result_idx.unwrap();
        let mut result_free_range = result_free_range.unwrap();
        result_free_range.end = result_free_range.start + size;
        Self::free_list_update_range(free_list, index, result_free_range);
        //println!("[1] result free range = {:?}", result_free_range);
        return Ok(result_free_range);
    }

    fn free_list_update_range(
        mut free_list: SgxMutexGuard<Vec<VMRange>>,
        index: usize,
        range: VMRange,
    ) {
        let ranges_after_subtraction = free_list[index].subtract(&range);
        debug_assert!(ranges_after_subtraction.len() <= 2);
        if ranges_after_subtraction.len() == 0 {
            free_list.remove(index);
            return;
        }
        free_list[index] = ranges_after_subtraction[0];
        if ranges_after_subtraction.len() == 2 {
            free_list.insert(index + 1, ranges_after_subtraction[1]);
        }
        // put small ranges to front
        free_list.sort_unstable_by(|range_a, range_b| range_a.size().cmp(&range_b.size()));
        //println!("[1] after mmap free range = {:?}", free_list);
    }

    pub fn add_clean_range_back_to_free_manager(&self, clean_range: VMRange) -> Result<()> {
        let mut free_list = self.free_manager.lock().unwrap();
        // TODO: This clean range should be inserted first to be allocated early.
        // However, insert has a huge performance impact in the benchmark. We can reconsider this in the future.
        free_list.push(clean_range);
        Ok(())
    }

    pub fn sort_when_exit(&self) {
        let mut free_list = self.free_manager.lock().unwrap();
        free_list.sort_unstable_by(|range_a, range_b| range_a.start().cmp(&range_b.start()));
        //println!("free_list when exit before merge = {:?}", free_list);
        while (free_list.len() != 1) {
            let right_range = free_list[1].clone();
            let mut left_range = &mut free_list[0];
            debug_assert!(left_range.end() == right_range.start());
            left_range.set_end(right_range.end());
            free_list.remove(1);
        }
        //println!("free_list when exit after merge = {:?}", free_list);
        debug_assert!(free_list.len() == 1);
    }

    // This can be called when there is not enough space for allocation or the thread is idle.
    pub fn sort_and_merge(&self) {
        let mut free_list = self.free_manager.lock().unwrap();
        if free_list.len() == 0 {
            return;
        }
        // sort 1st time to merge small ranges
        free_list.sort_unstable_by(|range_a, range_b| range_a.start().cmp(&range_b.start()));
        let mut idx = 0;
        while (idx < free_list.len() - 1) {
            let right_range = free_list[idx + 1].clone();
            let mut left_range = &mut free_list[idx];
            if left_range.end() == right_range.start() {
                left_range.set_end(right_range.end());
                free_list.remove(idx + 1);
                continue;
            }
            idx += 1;
        }

        // sort 2nd time to put small ranges to front
        free_list.sort_unstable_by(|range_a, range_b| range_a.size().cmp(&range_b.size()));
    }
}
