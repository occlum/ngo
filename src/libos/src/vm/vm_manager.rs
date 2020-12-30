use super::*;

use super::free_vm_manager::VMFreeSpaceManager;
use super::vm_area::*;
use super::vm_perms::VMPerms;
use crate::process::ThreadStatus;
use crate::time::timespec_t;
use core::ptr;
use intrusive_collections::rbtree::{Link, RBTree};
use intrusive_collections::Bound;
use intrusive_collections::RBTreeLink;
use intrusive_collections::{intrusive_adapter, KeyAdapter};
use sgx_tstd::sync::SgxThreadSpinlock;
use std::collections::HashSet;
use std::thread;
use std::time::Duration;
use vm_clean_thread::*;

#[derive(Clone, Debug)]
pub enum VMInitializer {
    DoNothing(),
    FillZeros(),
    CopyFrom { range: VMRange },
    LoadFromFile { file: FileRef, offset: usize },
}

impl Default for VMInitializer {
    fn default() -> VMInitializer {
        VMInitializer::DoNothing()
    }
}

impl VMInitializer {
    pub fn init_slice(&self, buf: &mut [u8]) -> Result<()> {
        match self {
            VMInitializer::DoNothing() => {
                // Do nothing
            }
            VMInitializer::FillZeros() => {
                for b in buf {
                    *b = 0;
                }
            }
            VMInitializer::CopyFrom { range } => {
                let src_slice = unsafe { range.as_slice() };
                let copy_len = min(buf.len(), src_slice.len());
                buf[..copy_len].copy_from_slice(&src_slice[..copy_len]);
                for b in &mut buf[copy_len..] {
                    *b = 0;
                }
            }
            VMInitializer::LoadFromFile { file, offset } => {
                // TODO: make sure that read_at does not move file cursor
                let len = file
                    .read_at(*offset, buf)
                    .cause_err(|_| errno!(EIO, "failed to init memory from file"))?;
                for b in &mut buf[len..] {
                    *b = 0;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VMMapAddr {
    Any,          // Free to choose any address
    Hint(usize),  // Prefer the address, but can use other address
    Need(usize),  // Need to use the address, otherwise report error
    Force(usize), // Force using the address by munmap first
}

impl Default for VMMapAddr {
    fn default() -> VMMapAddr {
        VMMapAddr::Any
    }
}

#[derive(Builder, Debug)]
#[builder(pattern = "owned", build_fn(skip), no_std)]
pub struct VMMapOptions {
    size: usize,
    align: usize,
    perms: VMPerms,
    addr: VMMapAddr,
    initializer: VMInitializer,
    // The content of the VMA can be written back to a given file at a given offset
    writeback_file: Option<(FileRef, usize)>,
}

// VMMapOptionsBuilder is generated automatically, except the build function
impl VMMapOptionsBuilder {
    pub fn build(mut self) -> Result<VMMapOptions> {
        let size = {
            let size = self
                .size
                .ok_or_else(|| errno!(EINVAL, "invalid size for mmap"))?;
            if size == 0 {
                return_errno!(EINVAL, "invalid size for mmap");
            }
            align_up(size, PAGE_SIZE)
        };
        let align = {
            let align = self.align.unwrap_or(PAGE_SIZE);
            if align == 0 || align % PAGE_SIZE != 0 {
                return_errno!(EINVAL, "invalid size for mmap");
            }
            align
        };
        let perms = self
            .perms
            .ok_or_else(|| errno!(EINVAL, "perms must be given"))?;
        let addr = {
            let addr = self.addr.unwrap_or_default();
            match addr {
                // TODO: check addr + size overflow
                VMMapAddr::Any => VMMapAddr::Any,
                VMMapAddr::Hint(addr) => {
                    let addr = align_down(addr, PAGE_SIZE);
                    VMMapAddr::Hint(addr)
                }
                VMMapAddr::Need(addr_) | VMMapAddr::Force(addr_) => {
                    if addr_ % align != 0 {
                        return_errno!(EINVAL, "unaligned addr for fixed mmap");
                    }
                    addr
                }
            }
        };
        let initializer = match self.initializer.as_ref() {
            Some(initializer) => initializer.clone(),
            None => VMInitializer::default(),
        };
        let writeback_file = self.writeback_file.take().unwrap_or_default();
        Ok(VMMapOptions {
            size,
            align,
            perms,
            addr,
            initializer,
            writeback_file,
        })
    }
}

impl VMMapOptions {
    pub fn size(&self) -> &usize {
        &self.size
    }

    pub fn addr(&self) -> &VMMapAddr {
        &self.addr
    }

    pub fn align(&self) -> &usize {
        &self.align
    }

    pub fn perms(&self) -> &VMPerms {
        &self.perms
    }

    pub fn initializer(&self) -> &VMInitializer {
        &self.initializer
    }

    pub fn writeback_file(&self) -> &Option<(FileRef, usize)> {
        &self.writeback_file
    }
}

#[derive(Debug)]
pub struct VMRemapOptions {
    old_addr: usize,
    old_size: usize,
    new_size: usize,
    flags: MRemapFlags,
}

impl VMRemapOptions {
    pub fn new(
        old_addr: usize,
        old_size: usize,
        new_size: usize,
        flags: MRemapFlags,
    ) -> Result<Self> {
        let old_addr = if old_addr % PAGE_SIZE != 0 {
            return_errno!(EINVAL, "unaligned old address");
        } else {
            old_addr
        };
        let old_size = if old_size == 0 {
            // TODO: support old_size is zero for shareable mapping
            warn!("do not support old_size is zero");
            return_errno!(EINVAL, "invalid old size");
        } else {
            align_up(old_size, PAGE_SIZE)
        };
        if let Some(new_addr) = flags.new_addr() {
            if new_addr % PAGE_SIZE != 0 {
                return_errno!(EINVAL, "unaligned new address");
            }
        }
        let new_size = if new_size == 0 {
            return_errno!(EINVAL, "invalid new size");
        } else {
            align_up(new_size, PAGE_SIZE)
        };
        Ok(Self {
            old_addr,
            old_size,
            new_size,
            flags,
        })
    }

    pub fn old_addr(&self) -> usize {
        self.old_addr
    }

    pub fn old_size(&self) -> usize {
        self.old_size
    }

    pub fn new_size(&self) -> usize {
        self.new_size
    }

    pub fn flags(&self) -> MRemapFlags {
        self.flags
    }
}

/// Memory manager.
///
/// VMManager provides useful memory management APIs such as mmap, munmap, mremap, etc. It also manages the whole
/// process VM including mmap, stack, heap, elf ranges.
///
/// # Invariants
///
/// Behind the scene, VMManager maintains a list of VMArea that have been allocated.
/// (denoted as `self.vmas`). To reason about the correctness of VMManager, we give
/// the set of invariants hold by VMManager.
///
/// 1. The rule of sentry:
/// ```
/// self.range.begin() == self.vmas[0].start() == self.vmas[0].end()
/// ```
/// and
/// ```
/// self.range.end() == self.vmas[N-1].start() == self.vmas[N-1].end()
/// ```
/// where `N = self.vmas.len()`.
///
/// 2. The rule of non-emptyness:
/// ```
/// self.vmas[i].size() > 0, for 1 <= i < self.vmas.len() - 1
/// ```
///
/// 3. The rule of ordering:
/// ```
/// self.vmas[i].end() <= self.vmas[i+1].start() for 0 <= i < self.vmas.len() - 1
/// ```
///
/// 4. The rule of non-mergablility:
/// ```
/// self.vmas[i].end() !=  self.vmas[i+1].start() || self.vmas[i].perms() !=  self.vmas[i+1].perms()
///     for 1 <= i < self.vmas.len() - 2
/// ```
///
#[derive(Debug, Default)]
pub struct VMManager {
    range: VMRange,
    vmas: SgxMutex<RBTree<VMAAdapter>>, // This almost gurantee the search/insert/delete to be O(logN)
    free: VMFreeSpaceManager,
    cleaning_ranges: SgxMutex<HashSet<VMRange>>, // we could have multiple cleaning threads (1 global cleaning thread, maybe several self cleaning when there's not enough free space)
    spin_lock: SpinLock,
}

struct SpinLock(SgxThreadSpinlock);

impl Debug for SpinLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "spin lock")
    }
}

impl Default for SpinLock {
    fn default() -> SpinLock {
        SpinLock(SgxThreadSpinlock::new())
    }
}

impl VMManager {
    pub fn from(addr: usize, size: usize) -> Result<VMManager> {
        let range = VMRange::new(addr, addr + size)?;
        let vmas = {
            let start = range.start();
            let end = range.end();
            let start_sentry = {
                let range = VMRange::new_empty(start)?;
                let perms = VMPerms::empty();
                VMArea::new(range, perms, None)
            };
            let end_sentry = {
                let range = VMRange::new_empty(end)?;
                let perms = VMPerms::empty();
                VMArea::new(range, perms, None)
            };
            let mut _vmas = RBTree::new(VMAAdapter::new());
            _vmas.insert(make_vma_obj(start_sentry));
            _vmas.insert(make_vma_obj(end_sentry));
            SgxMutex::new(_vmas)
        };

        let spin_lock = SpinLock::default();
        let free = VMFreeSpaceManager::new(range.clone());
        let cleaning_ranges = SgxMutex::new(HashSet::with_capacity(3));
        Ok(VMManager {
            range,
            vmas,
            free,
            cleaning_ranges,
            spin_lock,
        })
    }

    pub fn range(&self) -> &VMRange {
        &self.range
    }

    fn vmas(&self) -> &SgxMutex<RBTree<VMAAdapter>> {
        &self.vmas
    }

    pub fn free(&self) -> &VMFreeSpaceManager {
        &self.free
    }

    pub fn mmap(&self, mut options: VMMapOptions) -> Result<usize> {
        // TODO: respect options.align when mmap
        let addr = *options.addr();
        let size = *options.size();
        // TODO: support hint/force/need mmap options

        // free list and vmas must be updated together
        unsafe {
            self.spin_lock.0.lock();
        }
        // Allocate a new range for this mmap request
        let free_range = self.find_free_range(size, addr);
        if let Err(e) = free_range {
            // must unlock before return
            unsafe {
                self.spin_lock.0.unlock();
            }
            // let usage = self.usage_percentage();
            // println!("used size = 0x{:x}, total_size = 0x{:x}, used percentage = {} ", usage.0, usage.1, usage.2);
            return_errno!(e.errno(), "find free range error");
        }
        let new_free_range = free_range.unwrap();
        let new_addr = new_free_range.start();
        let writeback_file = options.writeback_file.take();
        let new_vma = make_vma_obj(VMArea::new(
            new_free_range,
            *options.perms(),
            writeback_file,
        ));

        // Initialize the memory of the new range
        unsafe {
            let buf = new_vma.vma.as_slice_mut();
            options.initializer.init_slice(buf)?;
        }
        // Set memory permissions
        if !options.perms.is_default() {
            Self::apply_perms(&new_vma.vma, new_vma.vma.perms());
        }

        // println!("mmap range: {:?}", new_free_range);
        // println!("free list range: {:?}", self.free);
        // After initializing, we can safely insert the new VMA
        //let mut insert_idx: usize = 0;
        //let vmas = self.vmas.lock().unwrap();
        self.vmas.lock().unwrap().insert(new_vma);
        unsafe {
            self.spin_lock.0.unlock();
        }
        //_println!("new vmas: {:?}", self.vmas.lock().unwrap());
        Ok(new_addr)
    }

    pub fn munmap(&self, addr: usize, size: usize) -> Result<()> {
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

        trace!("munmap range: {:?}", munmap_range);
        let bound = munmap_range.start();
        unsafe {
            self.spin_lock.0.lock();
        }
        let mut vmas = self.vmas.lock().unwrap();
        // the cursor to iterate vmas that might intersect with munmap_range
        let mut containing_vma = vmas.upper_bound_mut(Bound::Included(&bound));
        debug_assert!(containing_vma.get().unwrap().vma.start() <= bound);
        while !containing_vma.is_null()
            && containing_vma.get().unwrap().vma.start() <= munmap_range.end()
        {
            let vma = &containing_vma.get().unwrap().vma;
            if vma.size() == 0 {
                containing_vma.move_next();
                continue;
            }

            let intersection_vma = match vma.intersect(&munmap_range) {
                None => {
                    containing_vma.move_next();
                    continue;
                }
                Some(intersection_vma) => intersection_vma,
            };

            // File-backed VMA needs to be flushed upon munmap
            // TODO: make this async
            Self::flush_file_vma(&intersection_vma);

            if !&intersection_vma.perms().is_default() {
                Self::apply_perms(&intersection_vma, VMPerms::default());
            }

            CLEAN_REQ_QUEUE.send(intersection_vma.range().clone());

            if vma.range() == intersection_vma.range() {
                containing_vma.remove();
                continue;
            }

            let mut new_vmas = vma.subtract(&intersection_vma);
            if new_vmas.len() == 1 {
                let new_obj = make_vma_obj(new_vmas.pop().unwrap());
                containing_vma.replace_with(new_obj);
                containing_vma.move_next();
                continue;
            } else {
                // the intersection_vma is a subset of current vma
                debug_assert!(intersection_vma.is_subset_of(vma));
                let vma_left_part = make_vma_obj(new_vmas.swap_remove(0));
                containing_vma.replace_with(vma_left_part);
                let vma_right_part = make_vma_obj(new_vmas.pop().unwrap());
                vmas.insert(vma_right_part);
                break;
            }
        }

        drop(vmas);
        unsafe {
            self.spin_lock.0.unlock();
        }

        Ok(())
    }

    pub fn clean_dirty_range(&self, dirty_range: VMRange) -> Result<()> {
        let bound = dirty_range.start();

        //println!("bgthread cleaning range: {:?}", dirty_range);
        self.cleaning_ranges.lock().unwrap().insert(dirty_range);
        dirty_range.clean();
        unsafe {
            self.spin_lock.0.lock();
        }
        self.free.add_clean_range_back_to_free_manager(dirty_range);
        // println!("[_bgthread_] after clean free list: {:?}", self.free);
        self.cleaning_ranges.lock().unwrap().remove(&dirty_range);
        unsafe {
            self.spin_lock.0.unlock();
        };
        Ok(())
    }

    pub fn clean_dirty_range_safe(&self, dirty_range: VMRange) -> Result<()> {
        // Check if there is intersect part that is in vmas (using)
        let bound = dirty_range.start();
        // println!("dirty range = {:?}", dirty_range);
        let mut sub_dirty_ranges = vec![];
        let mut vmas = self.vmas.lock().unwrap();
        // println!("vmas shown cleaning thread = {:?}", vmas);
        let mut containing_vma = vmas.upper_bound_mut(Bound::Included(&bound));
        debug_assert!(containing_vma.get().unwrap().vma.start() <= bound);
        while !containing_vma.is_null()
            && containing_vma.get().unwrap().vma.start() <= dirty_range.end()
        {
            let vma = &containing_vma.get().unwrap().vma;
            if vma.size() == 0 {
                containing_vma.move_next();
                continue;
            }

            let intersection_vma = match vma.intersect(&dirty_range) {
                None => {
                    containing_vma.move_next();
                    continue;
                }
                Some(intersection_vma) => intersection_vma,
            };

            // the intersection vma has already mapped and is in used
            // println!("vma = {:?}, intersection_vma = {:?}", vma, intersection_vma);
            debug_assert!(dirty_range.is_superset_of(vma));
            sub_dirty_ranges = dirty_range.subtract(&intersection_vma);
        }
        drop(vmas);

        //println!("bgthread cleaning range: {:?}", dirty_range);
        sub_dirty_ranges.iter().for_each(|range| {
            self.cleaning_ranges.lock().unwrap().insert(*range);
            range.clean();
            unsafe {
                self.spin_lock.0.lock();
            }
            self.free.add_clean_range_back_to_free_manager(*range);
            // println!("[_bgthread_] after clean free list: {:?}", self.free);
            self.cleaning_ranges.lock().unwrap().remove(range);
            unsafe {
                self.spin_lock.0.unlock();
            }
        });
        Ok(())
    }

    pub async fn sort_when_exit(&self) -> Result<()> {
        // This shouldn't take long. As there could only be one last cleaning range.
        loop {
            let cleaning_ranges = self.cleaning_ranges.lock().unwrap();
            if cleaning_ranges.len() == 0 {
                self.free.sort_when_exit();
                break;
            }
        }
        Ok(())
    }

    pub fn find_free_range(&self, size: usize, addr: VMMapAddr) -> Result<VMRange> {
        let ret = self.free.find_free_range_internal(size, addr);
        if ret.is_ok() {
            return ret;
        }

        // Sadly, we can't find free range easily. We don't care about performance anymore when we hit here.
        // We just try to finish the request.
        // First, let's try to merge the free range to bigger range
        self.free.sort_and_merge();
        let ret = self.free.find_free_range_internal(size, addr);
        if ret.is_ok() {
            return ret;
        }

        // Second, become clean thread to clean munmap range
        // This will return when no more pending dirty ranges in the channel
        unsafe {
            self.spin_lock.0.unlock();
        }
        become_clean_thread();
        unsafe {
            self.spin_lock.0.lock();
        }
        let ret = self.free.find_free_range_internal(size, addr);
        if ret.is_ok() {
            return ret;
        }

        // For the last time, do sort and merge again
        self.free.sort_and_merge();
        self.free.find_free_range_internal(size, addr)
    }

    pub fn mremap(&self, options: &VMRemapOptions) -> Result<usize> {
        let old_addr = options.old_addr();
        let old_size = options.old_size();
        let old_range = VMRange::new_with_size(old_addr, old_size)?;
        let new_size = options.new_size();
        let flags = options.flags();

        #[derive(Clone, Copy, PartialEq)]
        enum SizeType {
            Same,
            Shrinking,
            Growing,
        };
        let size_type = if new_size == old_size {
            SizeType::Same
        } else if new_size < old_size {
            SizeType::Shrinking
        } else {
            SizeType::Growing
        };

        // Get the memory permissions of the old range
        let perms = {
            let bound = old_range.start();
            let vmas = self.vmas.lock().unwrap();
            let containing_vma = vmas.upper_bound(Bound::Included(&bound));
            if containing_vma.is_null()
                || !containing_vma.get().unwrap().vma.is_superset_of(&old_range)
            {
                return_errno!(EFAULT, "invalid range");
            }
            containing_vma.get().unwrap().vma.perms()
        };

        // Implement mremap as one optional mmap followed by one optional munmap.
        //
        // The exact arguments for the mmap and munmap are determined by the values of MRemapFlags
        // and SizeType. There is a total of 9 combinations between MRemapFlags and SizeType.
        // As some combinations result in the same mmap and munmap operations, the following code
        // only needs to match four patterns of (MRemapFlags, SizeType) and treat each case
        // accordingly.

        // Determine whether need to do mmap. And when possible, determine the returned address
        // TODO: should fill zeros even when extending a file-backed mapping?
        let (need_mmap, mut ret_addr) = match (flags, size_type) {
            (MRemapFlags::None, SizeType::Growing) => {
                let mmap_opts = VMMapOptionsBuilder::default()
                    .size(new_size - old_size)
                    .addr(VMMapAddr::Need(old_range.end()))
                    .perms(perms)
                    .initializer(VMInitializer::FillZeros())
                    .build()?;
                let ret_addr = Some(old_addr);
                (Some(mmap_opts), ret_addr)
            }
            (MRemapFlags::MayMove, SizeType::Growing) => {
                let prefered_new_range =
                    VMRange::new_with_size(old_addr + old_size, new_size - old_size)?;
                if self.is_free_range(&prefered_new_range) {
                    let mmap_ops = VMMapOptionsBuilder::default()
                        .size(prefered_new_range.size())
                        .addr(VMMapAddr::Need(prefered_new_range.start()))
                        .perms(perms)
                        .initializer(VMInitializer::FillZeros())
                        .build()?;
                    (Some(mmap_ops), Some(old_addr))
                } else {
                    let mmap_ops = VMMapOptionsBuilder::default()
                        .size(new_size)
                        .addr(VMMapAddr::Any)
                        .perms(perms)
                        .initializer(VMInitializer::CopyFrom { range: old_range })
                        .build()?;
                    // Cannot determine the returned address for now, which can only be obtained after calling mmap
                    let ret_addr = None;
                    (Some(mmap_ops), ret_addr)
                }
            }
            (MRemapFlags::FixedAddr(new_addr), _) => {
                let mmap_opts = VMMapOptionsBuilder::default()
                    .size(new_size)
                    .addr(VMMapAddr::Force(new_addr))
                    .perms(perms)
                    .initializer(VMInitializer::CopyFrom { range: old_range })
                    .build()?;
                let ret_addr = Some(new_addr);
                (Some(mmap_opts), ret_addr)
            }
            _ => (None, Some(old_addr)),
        };

        let need_munmap = match (flags, size_type) {
            (MRemapFlags::None, SizeType::Shrinking)
            | (MRemapFlags::MayMove, SizeType::Shrinking) => {
                let unmap_addr = old_addr + new_size;
                let unmap_size = old_size - new_size;
                Some((unmap_addr, unmap_size))
            }
            (MRemapFlags::MayMove, SizeType::Growing) => {
                if ret_addr.is_none() {
                    // We must need to do mmap. Thus unmap the old range
                    Some((old_addr, old_size))
                } else {
                    // We must choose to reuse the old range. Thus, no need to unmap
                    None
                }
            }
            (MRemapFlags::FixedAddr(new_addr), _) => {
                let new_range = VMRange::new_with_size(new_addr, new_size)?;
                if new_range.overlap_with(&old_range) {
                    return_errno!(EINVAL, "new range cannot overlap with the old one");
                }
                Some((old_addr, old_size))
            }
            _ => None,
        };

        // Perform mmap and munmap if needed
        if let Some(mmap_options) = need_mmap {
            let mmap_addr = self.mmap(mmap_options)?;

            if ret_addr.is_none() {
                ret_addr = Some(mmap_addr);
            }
        }
        if let Some((addr, size)) = need_munmap {
            self.munmap(addr, size).expect("never fail");
        }

        debug_assert!(ret_addr.is_some());
        Ok(ret_addr.unwrap())
    }

    pub fn mprotect(&self, addr: usize, size: usize, new_perms: VMPerms) -> Result<()> {
        let protect_range = VMRange::new_with_size(addr, size)?;

        unsafe {
            self.spin_lock.0.lock();
        }
        // FIXME: the current implementation requires the target range to be
        // contained in exact one VMA.
        let bound = protect_range.start();
        let mut vmas = self.vmas.lock().unwrap();
        //_println!("vmas before mprotect: {:?}", vmas);
        let mut vma_cursor = vmas.upper_bound_mut(Bound::Included(&bound));
        if vma_cursor.is_null() || !vma_cursor.get().unwrap().vma.is_superset_of(&protect_range) {
            return_errno!(EFAULT, "invalid range");
        }
        //_println!("containing_vma = {:?}", vma_cursor.get().unwrap().vma);

        let old_perms = vma_cursor.get().unwrap().vma.perms();
        if new_perms == old_perms {
            unsafe {
                self.spin_lock.0.unlock();
            }
            return Ok(());
        }

        let mut containing_vma = vma_cursor.get().unwrap().clone().vma;
        let same_start = protect_range.start() == containing_vma.start();
        let same_end = protect_range.end() == containing_vma.end();
        match (same_start, same_end) {
            (true, true) => {
                containing_vma.set_perms(new_perms);

                Self::apply_perms(&containing_vma, containing_vma.perms());
            }
            (false, true) => {
                containing_vma.set_end(protect_range.start());

                let new_vma = VMArea::inherits_file_from(&containing_vma, protect_range, new_perms);
                Self::apply_perms(&new_vma, new_vma.perms());
                // drop(vmas);
                vmas.insert(make_vma_obj(new_vma));
            }
            (true, false) => {
                containing_vma.set_start(protect_range.end());

                let new_vma = VMArea::inherits_file_from(&containing_vma, protect_range, new_perms);
                Self::apply_perms(&new_vma, new_vma.perms());
                // drop(vmas);
                vmas.insert(make_vma_obj(new_vma));
            }
            (false, false) => {
                // The containing VMA is divided into three VMAs:
                // Shrinked old VMA:    [containing_vma.start,     protect_range.start)
                // New VMA:             [protect_range.start,      protect_range.end)
                // Another new vma:     [protect_range.end,        containing_vma.end)

                let old_end = containing_vma.end();
                let protect_end = protect_range.end();

                // Shrinked old VMA
                containing_vma.set_end(protect_range.start());

                // New VMA
                let new_vma = VMArea::inherits_file_from(&containing_vma, protect_range, new_perms);
                Self::apply_perms(&new_vma, new_vma.perms());

                // Another new VMA
                let new_vma2 = {
                    let range = VMRange::new(protect_end, old_end).unwrap();
                    VMArea::inherits_file_from(&containing_vma, range, old_perms)
                };

                vma_cursor.replace_with(make_vma_obj(containing_vma));
                // drop(vmas);
                // let mut vmas = self.vmas.lock().unwrap();
                vmas.insert(make_vma_obj(new_vma));
                vmas.insert(make_vma_obj(new_vma2));
            }
        }
        //_println!("vmas after mprotect: {:?}", vmas);
        unsafe {
            self.spin_lock.0.unlock();
        }

        Ok(())
    }

    /// Sync all shared, file-backed memory mappings in the given range by flushing the
    /// memory content to its underlying file.
    pub fn msync_by_range(&self, sync_range: &VMRange) -> Result<()> {
        if !self.range().is_superset_of(&sync_range) {
            return_errno!(ENOMEM, "invalid range");
        }
        // This might give extra element but it is fine
        let low_bound = sync_range.start();
        let up_bound = sync_range.end();
        let vmas = self.vmas.lock().unwrap();
        let vmas_range = vmas.range(Bound::Included(&low_bound), Bound::Included(&up_bound));
        // ?FIXME: check if sync_range covers unmapped memory
        for (idx, adapter) in vmas_range.enumerate() {
            let vma = match adapter.vma.intersect(sync_range) {
                None => continue,
                Some(vma) => vma,
            };
            Self::flush_file_vma(&vma);
        }
        Ok(())
    }

    /// Sync all shared, file-backed memory mappings of the given file by flushing
    /// the memory content to the file.
    pub fn msync_by_file(&self, sync_file: &FileRef) {
        for vma_obj in self.vmas.lock().unwrap().iter() {
            let is_same_file = |file: &FileRef| -> bool { Arc::ptr_eq(&file, &sync_file) };
            Self::flush_file_vma_with_cond(&vma_obj.vma, is_same_file);
        }
    }

    /// Flush a file-backed VMA to its file. This has no effect on anonymous VMA.
    fn flush_file_vma(vma: &VMArea) {
        Self::flush_file_vma_with_cond(vma, |_| true)
    }

    /// Same as flush_vma, except that an extra condition on the file needs to satisfy.
    fn flush_file_vma_with_cond<F: Fn(&FileRef) -> bool>(vma: &VMArea, cond_fn: F) {
        let (file, file_offset) = match vma.writeback_file().as_ref() {
            None => return,
            Some((file_and_offset)) => file_and_offset,
        };
        let file_writable = file
            .access_mode()
            .map(|ac| ac.writable())
            .unwrap_or_default();
        if !file_writable {
            return;
        }
        if !cond_fn(file) {
            return;
        }
        file.write_at(*file_offset, unsafe { vma.as_slice() });
    }

    pub fn find_mmap_region(&self, addr: usize) -> Result<VMRange> {
        let vmas = self.vmas.lock().unwrap();
        let region = vmas.upper_bound(Bound::Included(&addr));
        if region.is_null() || !region.get().unwrap().vma.contains(addr) {
            return_errno!(ESRCH, "no mmap regions that contains the address");
        }
        return Ok(region.get().unwrap().vma.range().clone());
    }

    pub fn usage_percentage(&self) -> (usize, usize, f32) {
        let totol_size = self.range.size();
        let mut used_size = 0;
        self.vmas
            .lock()
            .unwrap()
            .iter()
            .for_each(|obj| used_size += obj.vma.size());

        return (used_size, totol_size, used_size as f32 / totol_size as f32);
    }

    // Returns whether the requested range is free
    fn is_free_range(&self, request_range: &VMRange) -> bool {
        trace!("mremap check free range: {:?}", self.free);
        trace!("mremap new request range: {:?}", request_range);
        self.range.is_superset_of(request_range)
            && self
                .free
                .inner()
                .iter()
                .any(|range| range.is_superset_of(request_range) == true)
    }

    fn can_merge_vmas(left: &VMArea, right: &VMArea) -> bool {
        debug_assert!(left.end() <= right.start());

        // Both of the two VMAs must not be sentry (whose size == 0)
        if left.size() == 0 || right.size() == 0 {
            return false;
        }
        // The two VMAs must border with each other
        if left.end() != right.start() {
            return false;
        }
        // The two VMAs must have the same memory permissions
        if left.perms() != right.perms() {
            return false;
        }

        // If the two VMAs have write-back files, the files must be the same and
        // the two file regions must be continuous.
        let left_writeback_file = left.writeback_file();
        let right_writeback_file = right.writeback_file();
        match (left_writeback_file, right_writeback_file) {
            (None, None) => true,
            (Some(_), None) => false,
            (None, Some(_)) => false,
            (Some((left_file, left_offset)), Some((right_file, right_offset))) => {
                Arc::ptr_eq(&left_file, &right_file)
                    && right_offset > left_offset
                    && right_offset - left_offset == left.size()
            }
        }
    }

    fn apply_perms(protect_range: &VMRange, perms: VMPerms) {
        extern "C" {
            pub fn occlum_ocall_mprotect(
                retval: *mut i32,
                addr: *const c_void,
                len: usize,
                prot: i32,
            ) -> sgx_status_t;
        };

        unsafe {
            let mut retval = 0;
            let addr = protect_range.start() as *const c_void;
            let len = protect_range.size();
            let prot = perms.bits() as i32;
            let sgx_status = occlum_ocall_mprotect(&mut retval, addr, len, prot);
            assert!(sgx_status == sgx_status_t::SGX_SUCCESS && retval == 0);
        }
    }
}

impl Drop for VMManager {
    fn drop(&mut self) {
        // Ensure that memory permissions are recovered
        for vma_obj in self.vmas.lock().unwrap().iter() {
            let vma = &vma_obj.vma;
            if vma.size() == 0 || vma.perms() == VMPerms::default() {
                continue;
            }
            Self::apply_perms(vma, VMPerms::default());
        }
    }
}
