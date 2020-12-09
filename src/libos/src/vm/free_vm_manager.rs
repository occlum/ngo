// Implements free space management for memory.
// Currently only use simple vector as the base structure.

use super::vm_manager::VMMapAddr;
use super::*;

static INITIAL_SIZE: usize = 100;

#[derive(Debug, Default)]
pub struct VMFreeSpaceManager {
    free_manager: SgxMutex<Vec<VMRange>>,
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

    // Find the free range that satisfies the constraints of size and address
    pub fn find_free_range(&self, size: usize, addr: VMMapAddr) -> Result<VMRange> {
        // Record the minimal free range that satisfies the contraints
        let mut result_free_range: Option<VMRange> = None;
        let mut result_idx: Option<usize> = None;
        let mut free_list = self.free_manager.lock().unwrap();
        //println!("1. free list when finding free range: {:?}", free_list);

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
                    if free_range.contains(addr) {
                        if free_range.end() - addr >= size {
                            free_range.start = addr;
                            free_range.end = addr + size;
                            Self::free_list_update_range(free_list, idx, free_range);
                            return Ok(free_range);
                        }
                    }
                }
                // Must have free_range.start == addr
                VMMapAddr::Need(addr) | VMMapAddr::Force(addr) => {
                    if free_range.start() > addr {
                        //unsafe {self.spin_lock.0.unlock();}
                        return_errno!(ENOMEM, "not enough memory for fixed mmap");
                    }
                    if !free_range.contains(addr) {
                        continue;
                    }
                    if free_range.end() - addr < size {
                        //unsafe {self.spin_lock.0.unlock();}
                        return_errno!(ENOMEM, "not enough memory for fixed mmap");
                    }
                    free_range.start = addr;
                    free_range.end = addr + size;
                    Self::free_list_update_range(free_list, idx, free_range);
                    return Ok(free_range);
                }
            }

            if result_free_range == None
                || result_free_range.as_ref().unwrap().size() > free_range.size()
            {
                result_free_range = Some(free_range);
                result_idx = Some(idx);
            }
        }

        if result_free_range.is_none() {
            //unsafe {self.spin_lock.0.unlock();}
            return_errno!(ENOMEM, "not enough memory");
        }

        let index = result_idx.unwrap();
        let mut result_free_range = result_free_range.unwrap();
        result_free_range.end = result_free_range.start + size;
        Self::free_list_update_range(free_list, index, result_free_range);
        return Ok(result_free_range);
    }

    fn free_list_update_range(
        mut free_list: SgxMutexGuard<Vec<VMRange>>,
        index: usize,
        range: VMRange,
    ) {
        let ranges_after_subtraction = free_list[index].subtract(&range);
        if ranges_after_subtraction.len() == 0 {
            free_list.remove(index);
            return;
        }
        free_list[index] = ranges_after_subtraction[0];
        if ranges_after_subtraction.len() > 1 {
            free_list.insert(index + 1, ranges_after_subtraction[1]);
        }
    }

    pub fn add_clean_range_back_to_free_manager(&self, clean_range: VMRange) -> Result<()> {
        let mut new_free_list = Vec::with_capacity(INITIAL_SIZE);
        let mut free_list = self.free_manager.lock().unwrap();
        //println!("1. dirty_range = {:?}", clean_range);
        //println!("1. before update free_list = {:?}", free_list);
        free_list.push(clean_range);
        free_list.sort_unstable_by(|range_a, range_b| range_a.start().cmp(&range_b.start()));

        new_free_list.push(free_list[0]);
        for i in 1..free_list.len() {
            let &mut top = new_free_list.as_mut_slice().last_mut().unwrap();

            if top.end() < free_list[i].start() {
                new_free_list.push(free_list[i]);
            } else if top.end() < free_list[i].end() {
                let mut new_top = top.clone();
                new_top.end = free_list[i].end();
                new_free_list.pop();
                new_free_list.push(new_top);
            }
        }
        *free_list = new_free_list;
        //println!("1. after update free_list = {:?}", free_list);
        Ok(())
    }
}
