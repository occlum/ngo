use super::*;
use std::alloc::{AllocErr, AllocRef, Layout};
use std::ptr::{self, write_bytes, NonNull};

pub const MAX_OCALL_ALLOC_SIZE: usize = 0x1_000 * 0x1_000; // 1MB

pub struct OcallAlloc;

unsafe impl AllocRef for OcallAlloc {
    fn alloc(&mut self, layout: Layout) -> std::result::Result<NonNull<[u8]>, AllocErr> {
        if layout.size() == 0 {
            return Err(AllocErr);
        }

        let layout = layout
            .align_to(std::mem::size_of::<*const c_void>())
            .unwrap();

        // default alignment of ocalloc is 16
        assert!(layout.align() <= 16);
        let mut mem_ptr = unsafe { sgx_ocalloc(layout.size()) } as *mut u8;
        if mem_ptr == std::ptr::null_mut() {
            return Err(AllocErr);
        }

        // Sanity checks
        // Post-condition 1: alignment
        debug_assert!(mem_ptr as usize % layout.align() == 0);
        // Post-condition 2: out-of-enclave
        assert!(sgx_trts::trts::rsgx_raw_is_outside_enclave(
            mem_ptr as *const u8,
            layout.size()
        ));
        Ok(NonNull::new(unsafe {
            core::slice::from_raw_parts_mut(mem_ptr, layout.size() as usize)
        })
        .unwrap())
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        // Pre-condition: out-of-enclave
        debug_assert!(sgx_trts::trts::rsgx_raw_is_outside_enclave(
            ptr.as_ptr(),
            layout.size()
        ));

        unsafe { sgx_ocfree() };
    }
}
