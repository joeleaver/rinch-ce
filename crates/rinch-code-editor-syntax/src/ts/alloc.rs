//! Routes tree-sitter's C allocations through Rust's global allocator on wasm.
//!
//! # Why this exists
//!
//! tree-sitter's wasm sysroot ships a bump allocator (`wasm/src/stdlib.c`) whose
//! `heap_start`/`next` pointers begin as NULL and are only initialized by
//! `reset_heap()` — which is called exclusively from the wasmtime sandbox host
//! path (`wasm_store.c`). On `wasm32-unknown-unknown` nothing ever calls it, so
//! the first `malloc` writes its header to address 0 and returns pointers that
//! overlap Rust's own stack and static data.
//!
//! **It fails silently at small sizes.** Parses below roughly 16 KB appear to
//! work; at 32 KB and above the module traps with `memory access out of bounds`
//! and stays corrupted afterward. That is why `tests/wasm_allocator.rs` parses a
//! deliberately large input — anything smaller passes even when broken.
//!
//! Upstream tracks this as tree-sitter issue #5530, still open. The obvious fix
//! (PR #5201, a Rust-backed allocator) was closed unmerged in January 2026; the
//! maintainers prefer to repair the C allocator for code-size reasons. Until
//! that lands, every wasm consumer must install its own allocator.
//!
//! # Safety contract
//!
//! `ts_set_allocator` swaps a global function table. Memory allocated by the old
//! allocator must never be freed by the new one, so [`install`] has to run
//! before tree-sitter allocates anything at all. Every public constructor in
//! this crate calls it first, and [`install`] is idempotent, so there is no path
//! by which a `Parser` can be created before the allocator is in place.

/// Install the Rust-backed allocator. Idempotent and cheap to call repeatedly.
///
/// On non-wasm targets this does nothing: the platform allocator is a real
/// `malloc` and is better tested than anything we would substitute for it.
#[inline]
pub fn install() {
    #[cfg(target_arch = "wasm32")]
    imp::install();
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use core::ffi::c_void;
    use std::alloc::Layout;
    use std::sync::Once;

    /// Bytes reserved before each payload to record its size. Must be at least
    /// `size_of::<usize>()` and a multiple of `ALIGN` so the payload stays aligned.
    const HEADER: usize = 16;
    /// Alignment handed to every allocation. 16 covers `max_align_t` on wasm32
    /// with room to spare, so a caller can never get an under-aligned pointer.
    const ALIGN: usize = 16;

    /// Build the layout for a payload of `size` bytes.
    ///
    /// Returns `None` on overflow rather than panicking: these functions are
    /// called from C, and unwinding across an FFI boundary is undefined
    /// behaviour. C's contract is to return NULL on failure, so we do that.
    fn layout_for(size: usize) -> Option<Layout> {
        let total = size.checked_add(HEADER)?;
        Layout::from_size_align(total, ALIGN).ok()
    }

    /// # Safety
    /// Returns a pointer to `size` writable bytes, or null. The caller must free
    /// it with [`ts_free`].
    unsafe fn alloc_impl(size: usize) -> *mut c_void {
        // C permits malloc(0) to return either NULL or a unique freeable
        // pointer. tree-sitter treats NULL as allocation failure, so return a
        // real one-byte allocation instead of risking a spurious OOM path.
        let size = size.max(1);
        let Some(layout) = layout_for(size) else {
            return core::ptr::null_mut();
        };
        let base = unsafe { std::alloc::alloc(layout) };
        if base.is_null() {
            return core::ptr::null_mut();
        }
        unsafe {
            (base as *mut usize).write(size);
            base.add(HEADER) as *mut c_void
        }
    }

    /// # Safety
    /// `ptr` must have come from [`alloc_impl`].
    unsafe fn payload_size(ptr: *mut c_void) -> usize {
        unsafe { ((ptr as *mut u8).sub(HEADER) as *mut usize).read() }
    }

    unsafe extern "C" fn ts_malloc(size: usize) -> *mut c_void {
        unsafe { alloc_impl(size) }
    }

    unsafe extern "C" fn ts_calloc(count: usize, size: usize) -> *mut c_void {
        let Some(total) = count.checked_mul(size) else {
            return core::ptr::null_mut();
        };
        let ptr = unsafe { alloc_impl(total) };
        if !ptr.is_null() {
            // Zero only what was asked for; alloc_impl may have rounded 0 up to 1.
            unsafe { core::ptr::write_bytes(ptr as *mut u8, 0, total) };
        }
        ptr
    }

    unsafe extern "C" fn ts_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            return unsafe { alloc_impl(size) };
        }
        if size == 0 {
            unsafe { ts_free(ptr) };
            return core::ptr::null_mut();
        }
        let old_size = unsafe { payload_size(ptr) };
        let new_ptr = unsafe { alloc_impl(size) };
        if new_ptr.is_null() {
            // C requires the original block to stay valid when realloc fails.
            return core::ptr::null_mut();
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                ptr as *const u8,
                new_ptr as *mut u8,
                old_size.min(size),
            );
            ts_free(ptr);
        }
        new_ptr
    }

    unsafe extern "C" fn ts_free(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let size = payload_size(ptr);
            // layout_for cannot fail here: this size already allocated once.
            if let Some(layout) = layout_for(size) {
                std::alloc::dealloc((ptr as *mut u8).sub(HEADER), layout);
            }
        }
    }

    static INSTALL: Once = Once::new();

    pub fn install() {
        INSTALL.call_once(|| unsafe {
            tree_sitter::set_allocator(
                Some(ts_malloc),
                Some(ts_calloc),
                Some(ts_realloc),
                Some(ts_free),
            );
        });
    }
}
