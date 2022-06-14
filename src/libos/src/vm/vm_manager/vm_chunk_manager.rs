use super::*;

use super::free_space_manager::VMFreeSpaceManager as FreeRangeManager;
use super::vm_area::*;
use super::vm_clean::CleanReq;
use super::vm_perms::VMPerms;
use super::vm_util::*;

use intrusive_collections::rbtree::RBTree;
use intrusive_collections::Bound;
use std::collections::HashSet;

const PROCESS_SET_INIT_SIZE: usize = 5;

pub type ChunkManagerRef = Arc<SgxMutex<ChunkManager>>;

/// Memory chunk manager.
///
/// Chunk is the memory unit for Occlum. For chunks with `default` size, every chunk is managed by a ChunkManager which provides
/// usedful memory management APIs such as mmap, munmap, mremap, mprotect, etc.
/// ChunkManager is implemented basically with two data structures: a red-black tree to track vmas in use and a FreeRangeManager to track
/// ranges which are free.
/// For vmas-in-use, there are two sentry vmas with zero length at the front and end of the red-black tree.
#[derive(Debug)]
pub struct ChunkManager {
    range: VMRange,
    vmas: RBTree<VMAAdapter>,
    free_manager: FreeManager,
    process_set: HashSet<pid_t>,
}

impl ChunkManager {
    pub fn from(addr: usize, size: usize) -> Result<ChunkManagerRef> {
        let range = VMRange::new(addr, addr + size)?;
        let vmas = {
            let start = range.start();
            let end = range.end();
            let start_sentry = {
                let range = VMRange::new_empty(start)?;
                let perms = VMPerms::empty();
                // sentry vma shouldn't belong to any process
                VMAObj::new_vma_obj(VMArea::new(range, perms, None, 0))
            };
            let end_sentry = {
                let range = VMRange::new_empty(end)?;
                let perms = VMPerms::empty();
                VMAObj::new_vma_obj(VMArea::new(range, perms, None, 0))
            };
            let mut new_tree = RBTree::new(VMAAdapter::new());
            new_tree.insert(start_sentry);
            new_tree.insert(end_sentry);
            new_tree
        };
        let mut process_set = HashSet::with_capacity(PROCESS_SET_INIT_SIZE);
        process_set.insert(current!().process().pid());

        let mut manager = Self {
            range,
            vmas,
            free_manager: FreeManager::new(&range),
            process_set,
        };

        let mut arc_self = Arc::new(SgxMutex::new(manager));
        unsafe {
            Arc::get_mut_unchecked(&mut arc_self)
                .lock()
                .unwrap()
                .free_manager
                .arc_self = Some(arc_self.clone());
        }
        Ok(arc_self)
    }

    pub fn new(vm_range: VMRange) -> Result<ChunkManagerRef> {
        ChunkManager::from(vm_range.start(), vm_range.size())
    }

    pub fn add_process(&mut self, pid: pid_t) {
        self.process_set.insert(pid);
    }

    pub fn is_owned_by_current_process(&self) -> bool {
        let current_pid = current!().process().pid();
        self.process_set.contains(&current_pid) && self.process_set.len() == 1
    }

    // Clean vmas when munmap a MultiVMA chunk, return whether this chunk is cleaned
    pub fn clean_multi_vmas(&mut self) -> bool {
        let current_pid = current!().process().pid();
        self.clean_vmas_with_pid(current_pid);
        self.process_set.remove(&current_pid);
        if self.is_empty() {
            return true;
        } else {
            return false;
        }
    }

    pub fn range(&self) -> &VMRange {
        &self.range
    }

    pub fn vmas(&self) -> &RBTree<VMAAdapter> {
        &self.vmas
    }

    pub fn free_size(&self) -> usize {
        self.free_manager.free_size
    }

    pub fn process_set(&mut self) -> &mut HashSet<pid_t> {
        &mut self.process_set
    }

    pub fn is_empty(&self) -> bool {
        self.free_size() == self.range.size()
    }

    pub fn return_clean_vm(&mut self, clean_range: &VMRange) -> Result<()> {
        self.free_manager.return_clean_vm(clean_range)
    }

    // Clean vmas that are not munmap-ed by user before exiting.
    fn clean_vmas_with_pid(&mut self, pid: pid_t) {
        let mut vmas_cursor = self.vmas.cursor_mut();
        vmas_cursor.move_next(); // move to the first element of the tree
        while !vmas_cursor.is_null() {
            let vma = vmas_cursor.get().unwrap().vma();
            if vma.pid() != pid || vma.size() == 0 {
                // Skip vmas which doesn't belong to this process
                vmas_cursor.move_next();
                continue;
            }

            Self::flush_file_vma(vma);

            if !vma.perms().is_default() {
                VMPerms::apply_perms(vma, VMPerms::default());
            }

            // This function is normally called when trying to drop this default chunk but there is vma that is not munmap-ed by user.
            // Here, we don't do the async way because when the cleaning is finished, the VMManager has missed the time to recycle this chunk.

            unsafe {
                vma.clean();
            }
            self.free_manager.return_clean_vm(vma.range());

            // Remove this vma from vmas list
            vmas_cursor.remove();
        }
    }

    pub fn mmap(&mut self, options: &VMMapOptions) -> Result<usize> {
        let addr = *options.addr();
        let size = *options.size();
        let align = *options.align();

        // Find and allocate a new range for this mmap request
        let new_range = self.free_manager.inner.find_free_range(size, align, addr)?;
        let new_addr = new_range.start();
        let writeback_file = options.writeback_file().clone();
        let current_pid = current!().process().pid();
        let new_vma = VMArea::new(new_range, *options.perms(), writeback_file, current_pid);

        // Initialize the memory of the new range
        let buf = unsafe { new_vma.as_slice_mut() };
        let ret = options.initializer().init_slice(buf);
        if let Err(e) = ret {
            // Return the free range before return with error
            self.free_manager
                .inner
                .add_range_back_to_free_manager(new_vma.range());
            return_errno!(e.errno(), "failed to mmap");
        }

        // Set memory permissions
        if !options.perms().is_default() {
            VMPerms::apply_perms(&new_vma, new_vma.perms());
        }
        self.free_manager.free_size -= new_vma.size();
        // After initializing, we can safely insert the new VMA
        self.vmas.insert(VMAObj::new_vma_obj(new_vma));
        Ok(new_addr)
    }

    pub fn munmap(&mut self, addr: usize, size: usize) -> Result<()> {
        let size = {
            if size == 0 {
                return_errno!(EINVAL, "size of munmap must not be zero");
            }
            align_up(size, PAGE_SIZE)
        };
        let munmap_range = {
            let munmap_range = VMRange::new(addr, addr + size)?;

            let effective_munmap_range_opt = munmap_range.intersect(&self.range);
            if effective_munmap_range_opt.is_none() {
                return Ok(());
            }

            let effective_munmap_range = effective_munmap_range_opt.unwrap();
            if effective_munmap_range.empty() {
                return Ok(());
            }
            effective_munmap_range
        };

        self.munmap_range(munmap_range)
    }

    pub fn munmap_range(&mut self, range: VMRange) -> Result<()> {
        let bound = range.start();
        let current_pid = current!().process().pid();

        // The cursor to iterate vmas that might intersect with munmap_range.
        // Upper bound returns the vma whose start address is below and nearest to the munmap range. Start from this range.
        let mut vmas_cursor = self.vmas.upper_bound_mut(Bound::Included(&bound));
        while !vmas_cursor.is_null() && vmas_cursor.get().unwrap().vma().start() <= range.end() {
            let vma = &vmas_cursor.get().unwrap().vma();
            if vma.size() == 0 || current_pid != vma.pid() {
                vmas_cursor.move_next();
                continue;
            }
            let intersection_vma = match vma.intersect(&range) {
                None => {
                    vmas_cursor.move_next();
                    continue;
                }
                Some(intersection_vma) => intersection_vma,
            };

            // File-backed VMA needs to be flushed upon munmap
            Self::flush_file_vma(&intersection_vma);
            if !&intersection_vma.perms().is_default() {
                VMPerms::apply_perms(&intersection_vma, VMPerms::default());
            }

            if vma.range() == intersection_vma.range() {
                // Exact match. Just remove.
                vmas_cursor.remove();
            } else {
                // The intersection_vma is a subset of current vma
                let mut remain_vmas = vma.subtract(&intersection_vma);
                if remain_vmas.len() == 1 {
                    let new_obj = VMAObj::new_vma_obj(remain_vmas.pop().unwrap());
                    vmas_cursor.replace_with(new_obj);
                    vmas_cursor.move_next();
                } else {
                    debug_assert!(remain_vmas.len() == 2);
                    let vma_left_part = VMAObj::new_vma_obj(remain_vmas.swap_remove(0));
                    vmas_cursor.replace_with(vma_left_part);
                    let vma_right_part = VMAObj::new_vma_obj(remain_vmas.pop().unwrap());
                    // The new element will be inserted at the correct position in the tree based on its key automatically.
                    vmas_cursor.insert(vma_right_part);
                }
            }

            self.free_manager
                .clean_dirty_range_and_return_back(&intersection_vma);
        }
        Ok(())
    }

    pub fn parse_mremap_options(&mut self, options: &VMRemapOptions) -> Result<VMRemapResult> {
        let old_addr = options.old_addr();
        let old_size = options.old_size();
        let old_range = VMRange::new_with_size(old_addr, old_size)?;
        let new_size = options.new_size();
        let flags = options.flags();
        let size_type = VMRemapSizeType::new(&old_size, &new_size);
        let current_pid = current!().process().pid();

        // Merge all connecting VMAs here because the old ranges must corresponds to one VMA
        self.merge_all_vmas();

        let containing_vma = {
            let bound = old_range.start();
            // Get the VMA whose start address is smaller but closest to the old range's start address
            let mut vmas_cursor = self.vmas.upper_bound_mut(Bound::Included(&bound));
            while !vmas_cursor.is_null()
                && vmas_cursor.get().unwrap().vma().start() <= old_range.end()
            {
                let vma = &vmas_cursor.get().unwrap().vma();
                // The old range must be contained in one single VMA
                if vma.pid() == current_pid && vma.is_superset_of(&old_range) {
                    break;
                } else {
                    vmas_cursor.move_next();
                    continue;
                }
            }
            if vmas_cursor.is_null() {
                return_errno!(EFAULT, "old range is not a valid vma range");
            }
            vmas_cursor.get().unwrap().vma().clone()
        };

        return self.parse(options, &containing_vma);
    }

    pub fn mprotect(&mut self, addr: usize, size: usize, new_perms: VMPerms) -> Result<()> {
        let protect_range = VMRange::new_with_size(addr, size)?;
        let bound = protect_range.start();
        let mut containing_vmas = self.vmas.upper_bound_mut(Bound::Included(&bound));
        if containing_vmas.is_null() {
            return_errno!(ENOMEM, "invalid range");
        }
        let current_pid = current!().process().pid();

        // If a mprotect range is not a subrange of one vma, it must be subrange of multiple connecting vmas.
        while !containing_vmas.is_null()
            && containing_vmas.get().unwrap().vma().start() <= protect_range.end()
        {
            let mut containing_vma = containing_vmas.get().unwrap().vma().clone();
            if containing_vma.pid() != current_pid {
                containing_vmas.move_next();
                continue;
            }

            let old_perms = containing_vma.perms();
            if new_perms == old_perms {
                containing_vmas.move_next();
                continue;
            }

            let intersection_vma = match containing_vma.intersect(&protect_range) {
                None => {
                    containing_vmas.move_next();
                    continue;
                }
                Some(intersection_vma) => intersection_vma,
            };

            if intersection_vma.range() == containing_vma.range() {
                // The whole containing_vma is mprotected
                containing_vma.set_perms(new_perms);
                VMPerms::apply_perms(&containing_vma, containing_vma.perms());
                containing_vmas.replace_with(VMAObj::new_vma_obj(containing_vma));
                containing_vmas.move_next();
                continue;
            } else {
                // A subrange of containing_vma is mprotected
                debug_assert!(containing_vma.is_superset_of(&intersection_vma));
                let mut remain_vmas = containing_vma.subtract(&intersection_vma);
                match remain_vmas.len() {
                    2 => {
                        // The containing VMA is divided into three VMAs:
                        // Shrinked old VMA:    [containing_vma.start,     protect_range.start)
                        // New VMA:             [protect_range.start,      protect_range.end)
                        // Another new vma:     [protect_range.end,        containing_vma.end)
                        let old_end = containing_vma.end();
                        let protect_end = protect_range.end();

                        // Shrinked old VMA
                        containing_vma.set_end(protect_range.start());

                        // New VMA
                        let new_vma = VMArea::inherits_file_from(
                            &containing_vma,
                            protect_range,
                            new_perms,
                            current_pid,
                        );
                        VMPerms::apply_perms(&new_vma, new_vma.perms());
                        let new_vma = VMAObj::new_vma_obj(new_vma);

                        // Another new VMA
                        let new_vma2 = {
                            let range = VMRange::new(protect_end, old_end).unwrap();
                            let new_vma = VMArea::inherits_file_from(
                                &containing_vma,
                                range,
                                old_perms,
                                current_pid,
                            );
                            VMAObj::new_vma_obj(new_vma)
                        };

                        containing_vmas.replace_with(VMAObj::new_vma_obj(containing_vma));
                        containing_vmas.insert(new_vma);
                        containing_vmas.insert(new_vma2);
                        // In this case, there is no need to check other vmas.
                        break;
                    }
                    1 => {
                        let remain_vma = remain_vmas.pop().unwrap();
                        if remain_vma.start() == containing_vma.start() {
                            // mprotect right side of the vma
                            containing_vma.set_end(remain_vma.end());
                        } else {
                            // mprotect left side of the vma
                            debug_assert!(remain_vma.end() == containing_vma.end());
                            containing_vma.set_start(remain_vma.start());
                        }
                        let new_vma = VMArea::inherits_file_from(
                            &containing_vma,
                            intersection_vma.range().clone(),
                            new_perms,
                            current_pid,
                        );
                        VMPerms::apply_perms(&new_vma, new_vma.perms());

                        containing_vmas.replace_with(VMAObj::new_vma_obj(containing_vma));
                        containing_vmas.insert(VMAObj::new_vma_obj(new_vma));
                        containing_vmas.move_next();
                        continue;
                    }
                    _ => unreachable!(),
                }
            }
        }

        Ok(())
    }

    /// Sync all shared, file-backed memory mappings in the given range by flushing the
    /// memory content to its underlying file.
    pub fn msync_by_range(&mut self, sync_range: &VMRange) -> Result<()> {
        if !self.range().is_superset_of(sync_range) {
            return_errno!(ENOMEM, "invalid range");
        }

        // ?FIXME: check if sync_range covers unmapped memory
        for vma_obj in &self.vmas {
            let vma = match vma_obj.vma().intersect(sync_range) {
                None => continue,
                Some(vma) => vma,
            };
            Self::flush_file_vma(&vma);
        }
        Ok(())
    }

    /// Sync all shared, file-backed memory mappings of the given file by flushing
    /// the memory content to the file.
    pub fn msync_by_file(&mut self, sync_file: &FileRef) {
        let is_same_file = |file: &FileRef| -> bool { file == sync_file };
        for vma_obj in &self.vmas {
            Self::flush_file_vma_with_cond(&vma_obj.vma(), is_same_file);
        }
    }

    /// Flush a file-backed VMA to its file. This has no effect on anonymous VMA.
    pub fn flush_file_vma(vma: &VMArea) {
        Self::flush_file_vma_with_cond(vma, |_| true)
    }

    /// Same as flush_vma, except that an extra condition on the file needs to satisfy.
    pub fn flush_file_vma_with_cond<F: Fn(&FileRef) -> bool>(vma: &VMArea, cond_fn: F) {
        let (file, file_offset) = match vma.writeback_file().as_ref() {
            None => return,
            Some((file_and_offset)) => file_and_offset,
        };
        let inode_file = file.as_inode_file().unwrap();
        let file_writable = inode_file.access_mode().writable();
        if !file_writable {
            return;
        }
        if !cond_fn(file) {
            return;
        }
        inode_file.write_at(*file_offset, unsafe { vma.as_slice() });
    }

    pub fn find_mmap_region(&self, addr: usize) -> Result<VMRange> {
        let vma = self.vmas.upper_bound(Bound::Included(&addr));
        if vma.is_null() {
            return_errno!(ESRCH, "no mmap regions that contains the address");
        }
        let vma = vma.get().unwrap().vma();
        if vma.pid() != current!().process().pid() || !vma.contains(addr) {
            return_errno!(ESRCH, "no mmap regions that contains the address");
        }

        return Ok(vma.range().clone());
    }

    pub fn usage_percentage(&self) -> f32 {
        let totol_size = self.range.size();
        let mut used_size = 0;
        self.vmas
            .iter()
            .for_each(|vma_obj| used_size += vma_obj.vma().size());

        return used_size as f32 / totol_size as f32;
    }

    fn merge_all_vmas(&mut self) {
        let mut vmas_cursor = self.vmas.cursor_mut();
        vmas_cursor.move_next(); // move to the first element of the tree
        while !vmas_cursor.is_null() {
            let vma_a = vmas_cursor.get().unwrap().vma();
            if vma_a.size() == 0 {
                vmas_cursor.move_next();
                continue;
            }

            // Peek next, don't move the cursor
            let vma_b = vmas_cursor.peek_next().get().unwrap().vma().clone();
            if VMArea::can_merge_vmas(vma_a, &vma_b) {
                let merged_vmas = {
                    let mut new_vma = vma_a.clone();
                    new_vma.set_end(vma_b.end());
                    new_vma
                };
                let new_obj = VMAObj::new_vma_obj(merged_vmas);
                vmas_cursor.replace_with(new_obj);
                // Move cursor to vma_b
                vmas_cursor.move_next();
                let removed_vma = *vmas_cursor.remove().unwrap();
                debug_assert!(removed_vma.vma().is_the_same_to(&vma_b));

                // Remove operations makes the cursor go to next element. Move it back
                vmas_cursor.move_prev();
            } else {
                // Can't merge these two vma, just move to next
                vmas_cursor.move_next();
                continue;
            }
        }
    }

    // Returns whether the requested range is free
    fn is_free_range(&self, request_range: &VMRange) -> bool {
        self.free_manager.inner.is_free_range(request_range)
    }
}

impl VMRemapParser for ChunkManager {
    fn is_free_range(&self, request_range: &VMRange) -> bool {
        self.is_free_range(request_range)
    }
}

impl Drop for ChunkManager {
    fn drop(&mut self) {
        assert!(self.is_empty());
        assert!(self.free_manager.free_size == self.range.size());
        assert!(self.free_manager.inner.free_size() == self.range.size());
    }
}

#[derive(Debug)]
struct FreeManager {
    free_size: usize,
    arc_self: Option<Arc<SgxMutex<ChunkManager>>>,
    inner: FreeRangeManager,
}

impl FreeManager {
    fn new(range: &VMRange) -> Self {
        Self {
            free_size: range.size(),
            arc_self: None,
            inner: FreeRangeManager::new(range.clone()),
        }
    }

    fn return_clean_vm(&mut self, clean_range: &VMRange) -> Result<()> {
        self.inner.add_range_back_to_free_manager(clean_range)?;
        self.free_size += clean_range.size();
        Ok(())
    }

    // These requests are either send to clean queue or clean by current thread
    fn clean_dirty_range_and_return_back(&mut self, target_vma: &VMArea) {
        if CLEAN_QUEUE.is_clean_worker_needed(target_vma.size()) {
            let clean_reqs = CleanReq::new_reqs(target_vma.range().clone(), self.arc_self.clone());
            if CLEAN_QUEUE.send_reqs(clean_reqs).is_ok() {
                return;
            }
        }

        unsafe { target_vma.clean() };
        self.return_clean_vm(target_vma);
    }
}
