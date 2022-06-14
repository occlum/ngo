use super::*;

use super::vm_area::VMArea;
use super::vm_chunk_manager::ChunkManager;
use super::vm_perms::VMPerms;
use super::vm_util::*;
use crate::process::ProcessRef;
use crate::process::ThreadRef;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use vm_chunk_manager::ChunkManagerRef;

// For single VMA chunk, the vma struct doesn't need to update the pid field. Because all the chunks are recorded by the process VM already.
pub const DUMMY_CHUNK_PROCESS_ID: pid_t = 0;
// Default chunk size: 32MB
pub const CHUNK_DEFAULT_SIZE: usize = 32 * 1024 * 1024;

pub type ChunkID = usize;
pub type ChunkRef = Arc<Chunk>;

pub struct Chunk {
    // This range is used for fast check without any locks. However, when mremap, the size of this range could be
    // different with the internal VMA range for single VMA chunk. This can only be corrected by getting the internal
    // VMA, creating a new chunk and replacing the old chunk.
    range: VMRange,
    internal: ChunkType,
}

impl Hash for Chunk {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.range.hash(state);
    }
}

impl Ord for Chunk {
    fn cmp(&self, other: &Self) -> Ordering {
        self.range.start().cmp(&other.range.start())
    }
}

impl PartialOrd for Chunk {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Chunk {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
    }
}

impl Eq for Chunk {}

impl Debug for Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.internal() {
            ChunkType::SingleVMA(vma) => write!(f, "Single VMA chunk: {:?}", vma),
            ChunkType::MultiVMA(internal_manager) => write!(f, "default chunk: {:?}", self.range()),
        }
    }
}

impl Chunk {
    pub fn range(&self) -> &VMRange {
        &self.range
    }

    pub(super) fn internal(&self) -> &ChunkType {
        &self.internal
    }

    pub fn is_empty(&self) -> bool {
        match self.internal() {
            ChunkType::MultiVMA(internal_manager) => internal_manager.lock().unwrap().is_empty(),
            ChunkType::SingleVMA(_) => false,
        }
    }

    pub fn get_vma_for_single_vma_chunk(&self) -> VMArea {
        match self.internal() {
            ChunkType::MultiVMA(internal_manager) => unreachable!(),
            ChunkType::SingleVMA(vma) => return vma.lock().unwrap().clone(),
        }
    }

    pub fn free_size(&self) -> usize {
        match self.internal() {
            ChunkType::SingleVMA(vma) => 0, // for single VMA chunk, there is no free space
            ChunkType::MultiVMA(internal_manager) => internal_manager.lock().unwrap().free_size(),
        }
    }

    pub fn new_default_chunk(vm_range: VMRange) -> Result<Self> {
        Ok(Self {
            range: vm_range,
            internal: ChunkType::MultiVMA(ChunkManager::new(vm_range)?),
        })
    }

    pub fn new_single_vma_chunk(vm_range: &VMRange, options: &VMMapOptions) -> Result<Self> {
        let writeback_file = options.writeback_file().clone();
        let vm_area = VMArea::new(
            vm_range.clone(),
            *options.perms(),
            writeback_file,
            DUMMY_CHUNK_PROCESS_ID,
        );
        // Initialize the memory of the new range
        unsafe {
            let buf = vm_range.as_slice_mut();
            options.initializer().init_slice(buf)?;
        }
        // Set memory permissions
        if !options.perms().is_default() {
            VMPerms::apply_perms(&vm_area, vm_area.perms());
        }
        Ok(Self::new_chunk_with_vma(vm_area))
    }

    pub fn new_chunk_with_vma(vma: VMArea) -> Self {
        Self {
            range: vma.range().clone(),
            internal: ChunkType::SingleVMA(SgxMutex::new(vma)),
        }
    }

    pub fn is_owned_by_current_process(&self) -> bool {
        let current = current!();
        let process_mem_chunks = current.vm().mem_chunks().0.read().unwrap();
        if !process_mem_chunks
            .iter()
            .any(|chunk| chunk.range() == self.range())
        {
            return false;
        }

        match self.internal() {
            ChunkType::SingleVMA(vma) => true,
            ChunkType::MultiVMA(internal_manager) => {
                let internal_manager = internal_manager.lock().unwrap();
                internal_manager.is_owned_by_current_process()
            }
        }
    }

    pub fn add_process(&self, current: &ThreadRef) {
        match self.internal() {
            ChunkType::SingleVMA(vma) => unreachable!(),
            ChunkType::MultiVMA(internal_manager) => {
                internal_manager
                    .lock()
                    .unwrap()
                    .add_process(current.process().pid());
            }
        }
    }

    pub fn mmap(&self, options: &VMMapOptions) -> Result<usize> {
        debug_assert!(!self.is_single_vma());
        trace!("try allocate in chunk: {:?}", self);
        let mut internal_manager = if let ChunkType::MultiVMA(internal_manager) = &self.internal {
            internal_manager.lock().unwrap()
        } else {
            unreachable!();
        };
        if &internal_manager.free_size() < options.size() {
            return_errno!(ENOMEM, "no enough size without trying. try other chunks");
        }
        return internal_manager.mmap(options);
    }

    pub fn try_mmap(&self, options: &VMMapOptions) -> Result<usize> {
        debug_assert!(!self.is_single_vma());
        // Try lock ChunkManager. If it fails, just return and will try other chunks.
        let mut internal_manager = if let ChunkType::MultiVMA(internal_manager) = &self.internal {
            internal_manager
                .try_lock()
                .map_err(|_| errno!(EAGAIN, "try other chunks"))?
        } else {
            unreachable!();
        };
        trace!("get lock, try mmap in chunk: {:?}", self);
        if &internal_manager.free_size() < options.size() {
            return_errno!(ENOMEM, "no enough size without trying. try other chunks");
        }
        internal_manager.mmap(options)
    }

    pub fn is_single_vma(&self) -> bool {
        if let ChunkType::SingleVMA(_) = self.internal {
            true
        } else {
            false
        }
    }

    pub fn is_single_dummy_vma(&self) -> bool {
        if let ChunkType::SingleVMA(vma) = &self.internal {
            vma.lock().unwrap().size() == 0
        } else {
            false
        }
    }

    // Chunk size and internal VMA size are conflict.
    // This is due to the change of internal VMA.
    pub fn is_single_vma_with_conflict_size(&self) -> bool {
        if let ChunkType::SingleVMA(vma) = &self.internal {
            vma.lock().unwrap().size() != self.range.size()
        } else {
            false
        }
    }

    pub fn is_single_vma_chunk_should_be_removed(&self) -> bool {
        if let ChunkType::SingleVMA(vma) = &self.internal {
            let vma_size = vma.lock().unwrap().size();
            vma_size == 0 || vma_size != self.range.size()
        } else {
            false
        }
    }

    pub fn find_mmap_region(&self, addr: usize) -> Result<VMRange> {
        let internal = &self.internal;
        match self.internal() {
            ChunkType::SingleVMA(vma) => {
                let vma = vma.lock().unwrap();
                if vma.contains(addr) {
                    return Ok(vma.range().clone());
                } else {
                    return_errno!(ESRCH, "addr not found in this chunk")
                }
            }
            ChunkType::MultiVMA(internal_manager) => {
                return internal_manager.lock().unwrap().find_mmap_region(addr);
            }
        }
    }

    pub fn is_free_range(&self, request_range: &VMRange) -> bool {
        match self.internal() {
            ChunkType::SingleVMA(_) => false, // single-vma chunk can't be free
            ChunkType::MultiVMA(internal_manager) => internal_manager
                .lock()
                .unwrap()
                .is_free_range(request_range),
        }
    }
}

#[derive(Debug)]
pub(super) enum ChunkType {
    SingleVMA(SgxMutex<VMArea>),
    MultiVMA(ChunkManagerRef),
}

// MemChunks is the structure to track all the chunks which are used by this process.
#[derive(Debug)]
pub struct MemChunks(Arc<RwLock<HashSet<ChunkRef>>>);

impl Default for MemChunks {
    fn default() -> Self {
        MemChunks(Arc::new(RwLock::new(HashSet::new())))
    }
}

impl MemChunks {
    pub fn inner(&self) -> RwLockReadGuard<HashSet<ChunkRef>> {
        self.0.read().unwrap()
    }

    pub fn inner_mut(&self) -> RwLockWriteGuard<HashSet<ChunkRef>> {
        self.0.write().unwrap()
    }

    pub fn add_mem_chunk(&self, chunk: ChunkRef) {
        let mut mem_chunks = self.0.write().unwrap();
        mem_chunks.insert(chunk);
    }

    pub fn remove_mem_chunk(&self, chunk: &ChunkRef) {
        let mut mem_chunks = self.0.write().unwrap();
        mem_chunks.remove(chunk);
    }

    pub fn len(&self) -> usize {
        self.0.read().unwrap().len()
    }

    // Try merging all connecting single VMAs of the process.
    // This is a very expensive operation.
    pub fn merge_all_single_vma_chunks(&self) -> Result<Vec<VMArea>> {
        let mut mem_chunks = self.0.write().unwrap();
        let mut single_vma_chunks = mem_chunks
            .drain_filter(|chunk| chunk.is_single_vma())
            .collect::<Vec<ChunkRef>>();
        single_vma_chunks.sort_unstable_by(|chunk_a, chunk_b| {
            chunk_a
                .range()
                .start()
                .partial_cmp(&chunk_b.range().start())
                .unwrap()
        });

        // Try merging connecting VMAs
        for chunks in single_vma_chunks.windows(2) {
            let chunk_a = &chunks[0];
            let chunk_b = &chunks[1];
            let mut vma_a = match chunk_a.internal() {
                ChunkType::MultiVMA(_) => {
                    unreachable!();
                }
                ChunkType::SingleVMA(vma) => vma.lock().unwrap(),
            };

            let mut vma_b = match chunk_b.internal() {
                ChunkType::MultiVMA(_) => {
                    unreachable!();
                }
                ChunkType::SingleVMA(vma) => vma.lock().unwrap(),
            };

            if VMArea::can_merge_vmas(&vma_a, &vma_b) {
                let new_start = vma_a.start();
                vma_b.set_start(new_start);
                // set vma_a to zero
                vma_a.set_end(new_start);
            }
        }

        // Remove single dummy VMA chunk
        single_vma_chunks
            .drain_filter(|chunk| chunk.is_single_dummy_vma())
            .collect::<Vec<ChunkRef>>();

        // Get all merged chunks whose vma and range are conflict
        let merged_chunks = single_vma_chunks
            .drain_filter(|chunk| chunk.is_single_vma_with_conflict_size())
            .collect::<Vec<ChunkRef>>();

        // Get merged vmas
        let mut new_vmas = Vec::new();
        merged_chunks.iter().for_each(|chunk| {
            let vma = chunk.get_vma_for_single_vma_chunk();
            new_vmas.push(vma)
        });

        // Add all merged vmas back to mem_chunk list of the process
        new_vmas.iter().for_each(|vma| {
            let chunk = Arc::new(Chunk::new_chunk_with_vma(vma.clone()));
            mem_chunks.insert(chunk);
        });

        // Add all unchanged single vma chunks back to mem_chunk list
        while single_vma_chunks.len() > 0 {
            let chunk = single_vma_chunks.pop().unwrap();
            mem_chunks.insert(chunk);
        }

        Ok(new_vmas)
    }
}
