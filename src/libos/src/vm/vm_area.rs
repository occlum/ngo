use std::ops::{Deref, DerefMut};

use super::vm_perms::VMPerms;
use super::vm_range::VMRange;
use super::*;

use intrusive_collections::rbtree::{Link, RBTree};
use intrusive_collections::{intrusive_adapter, KeyAdapter};

#[derive(Clone, Debug, Default)]
pub struct VMArea {
    range: VMRange,
    perms: VMPerms,
    writeback_file: Option<(FileRef, usize)>,
    pid: pid_t,
}

impl VMArea {
    pub fn new(
        range: VMRange,
        perms: VMPerms,
        writeback_file: Option<(FileRef, usize)>,
        pid: pid_t,
    ) -> Self {
        Self {
            range,
            perms,
            writeback_file,
            pid,
        }
    }

    /// Create a new VMArea object that inherits the write-back file (if any), but has
    /// a new range and permissions.
    pub fn inherits_file_from(
        vma: &VMArea,
        new_range: VMRange,
        new_perms: VMPerms,
        pid: pid_t,
    ) -> Self {
        let new_writeback_file = vma.writeback_file.as_ref().map(|(file, file_offset)| {
            let new_file = file.clone();

            let new_file_offset = if vma.start() < new_range.start() {
                let vma_offset = new_range.start() - vma.start();
                *file_offset + vma_offset
            } else {
                let vma_offset = vma.start() - new_range.start();
                debug_assert!(*file_offset >= vma_offset);
                *file_offset - vma_offset
            };
            (new_file, new_file_offset)
        });
        Self::new(new_range, new_perms, new_writeback_file, pid)
    }

    pub fn perms(&self) -> VMPerms {
        self.perms
    }

    pub fn range(&self) -> &VMRange {
        &self.range
    }

    pub fn pid(&self) -> pid_t {
        self.pid
    }

    pub fn writeback_file(&self) -> &Option<(FileRef, usize)> {
        &self.writeback_file
    }

    pub fn set_perms(&mut self, new_perms: VMPerms) {
        self.perms = new_perms;
    }

    pub fn subtract(&self, other: &VMRange) -> Vec<VMArea> {
        self.deref()
            .subtract(other)
            .into_iter()
            .map(|range| Self::inherits_file_from(self, range, self.perms(), self.pid()))
            .collect()
    }

    // Returns an non-empty intersection if where is any
    pub fn intersect(&self, other: &VMRange) -> Option<VMArea> {
        let new_range = {
            let new_range = self.range().intersect(other);
            if new_range.is_none() {
                return None;
            }
            new_range.unwrap()
        };
        let new_vma = VMArea::inherits_file_from(self, new_range, self.perms(), self.pid());
        Some(new_vma)
    }

    pub fn resize(&mut self, new_size: usize) {
        self.range.resize(new_size)
    }

    pub fn set_start(&mut self, new_start: usize) {
        let old_start = self.start();
        self.range.set_start(new_start);

        // If the updates to the VMA needs to write back to a file, then the
        // file offset must be adjusted according to the new start address.
        if let Some((_, offset)) = self.writeback_file.as_mut() {
            if old_start < new_start {
                *offset += new_start - old_start;
            } else {
                // The caller must guarantee that the new start makes sense
                debug_assert!(*offset >= old_start - new_start);
                *offset -= old_start - new_start;
            }
        }
    }

    pub fn set_end(&mut self, new_end: usize) {
        self.range.set_end(new_end);
    }
}

impl Deref for VMArea {
    type Target = VMRange;

    fn deref(&self) -> &Self::Target {
        &self.range
    }
}

#[derive(Clone)]
pub struct VMAObj {
    link: Link,
    vma: VMArea,
}

impl fmt::Debug for VMAObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.vma)
    }
}

// key adapter for RBTree which is sorted by the start of vma ranges
intrusive_adapter!(pub VMAAdapter = Box<VMAObj>: VMAObj { link : Link });
impl<'a> KeyAdapter<'a> for VMAAdapter {
    type Key = usize;
    fn get_key(&self, vma_obj: &'a VMAObj) -> usize {
        vma_obj.vma.range().start()
    }
}

impl VMAObj {
    pub fn new_vma_obj(vma: VMArea) -> Box<Self> {
        Box::new(Self {
            link: Link::new(),
            vma,
        })
    }

    pub fn vma(&self) -> &VMArea {
        &self.vma
    }
}

impl VMArea {
    pub fn new_obj(
        range: VMRange,
        perms: VMPerms,
        writeback_file: Option<(FileRef, usize)>,
        pid: pid_t,
    ) -> Box<VMAObj> {
        Box::new(VMAObj {
            link: Link::new(),
            vma: VMArea {
                range,
                perms,
                writeback_file,
                pid,
            },
        })
    }
}
