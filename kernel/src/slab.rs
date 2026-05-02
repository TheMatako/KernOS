// kernel/src/slab.rs
//
// Slab allocator — Brick 4, part 2/2.
//
// ── What is a slab allocator? ─────────────────────────────────────────────────
//
// A slab allocator groups allocations of the same size into "slabs" — one
// or more 4 KiB pages carved into fixed-size "objects".  When you free an
// object, it goes back into the slab's free list and can be instantly reused
// the next time the same size is requested, with no fragmentation.
//
// This is exactly how the Linux kernel allocator (SLUB) and jemalloc work.
//
// ── Our design ───────────────────────────────────────────────────────────────
//
//   Size classes: 16, 32, 64, 128, 256, 512, 1024, 2048, 4096 bytes.
//   Each size class has one `SlabCache` that manages a linked list of slabs.
//   Each slab is one 4 KiB frame (from PMM), split into N objects of that size.
//
//   Object layout inside a slab:
//     ┌──────────────────────────────────────────────┐
//     │  Slab header (SlabHeader, at byte 0)         │
//     │  free_list : *mut FreeNode  ──┐              │
//     │  used      : usize            │              │
//     ├──────────────────────────────────────────────┤
//     │  object 0   ← FreeNode when free             │
//     │  object 1                                    │
//     │  …                                           │
//     └──────────────────────────────────────────────┘
//
//   A FreeNode is just a pointer to the next free object — we store it
//   *inside* the free object itself (intrusive free list).  Since the object
//   is free, its memory is unused, so we can borrow the first 8 bytes for
//   the pointer.
//
// ── kmalloc / kfree ──────────────────────────────────────────────────────────
//
//   `kmalloc(size)` → rounds `size` up to the next size class → calls
//                     `SlabCache::alloc()` for that class.
//
//   `kfree(ptr)`    → determines the slab from the pointer (floor to 4 KiB) →
//                     calls `SlabCache::free()` on the owning cache.
//
// ── Heap virtual addresses ────────────────────────────────────────────────────
//
//   Slabs live in the kernel heap virtual range defined in vmm.rs:
//     [KERNEL_HEAP_START, KERNEL_HEAP_START + KERNEL_HEAP_SIZE)
//
//   Each new slab frame is mapped at the next available address in this range
//   using `vmm::map_4k()`.  The backing physical frame comes from `pmm::alloc_frame()`.

#![allow(dead_code)]

use core::ptr;
use x86_64::{structures::paging::PageTableFlags, PhysAddr, VirtAddr};

use crate::{pmm, vmm};

// ---------------------------------------------------------------------------
// Size classes
// ---------------------------------------------------------------------------

/// All supported allocation sizes, in bytes.
///
/// Must be sorted ascending.  The smallest class (16 B) is large enough to
/// hold a pointer on a 64-bit system.
const SIZE_CLASSES: [usize; 9] = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096];

/// Number of size classes.
const NUM_CLASSES: usize = SIZE_CLASSES.len();

/// Page size (must match PMM frame size).
const PAGE_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Intrusive free list node
// ---------------------------------------------------------------------------

/// A node in the per-slab intrusive free list.
///
/// When an object is free, the first 8 bytes of its memory are interpreted
/// as a `FreeNode` (a pointer to the next free object in the same slab).
/// When the object is allocated, the caller may overwrite these bytes freely.
struct FreeNode {
    /// Pointer to the next free node, or null if this is the last one.
    next: *mut FreeNode,
}

// ---------------------------------------------------------------------------
// Slab header
// ---------------------------------------------------------------------------

/// Metadata stored at the very beginning of each slab page (byte 0).
///
/// The objects follow immediately after in memory.
///
/// `#[repr(C)]` guarantees that the compiler does not reorder fields, which
/// matters because we compute the offset of the first object as
/// `sizeof(SlabHeader)` rounded up to `object_size`.
#[repr(C)]
struct SlabHeader {
    /// Linked list pointer to the next slab in this cache.
    next_slab: *mut SlabHeader,
    /// Head of the intrusive free list for objects in this slab.
    free_list: *mut FreeNode,
    /// Number of objects currently allocated from this slab.
    used: usize,
    /// Number of objects this slab can hold in total.
    capacity: usize,
    /// Size of each object in bytes (= the size class).
    object_size: usize,
}

// ---------------------------------------------------------------------------
// Slab cache
// ---------------------------------------------------------------------------

/// Manages slabs for one size class.
struct SlabCache {
    /// Size class this cache serves (bytes per object).
    object_size: usize,
    /// Linked list of slab pages.  Head is the most recently added slab.
    head: *mut SlabHeader,
}

impl SlabCache {
    /// Creates an empty cache for the given object size.
    const fn new(object_size: usize) -> Self {
        Self {
            object_size,
            head: ptr::null_mut(),
        }
    }

    /// Allocates one object from this cache.
    ///
    /// If no slab has a free slot, a new slab is allocated from the PMM and
    /// mapped into the kernel heap virtual range.
    ///
    /// Returns a non-null pointer to the allocated object, or `None` if OOM.
    ///
    /// # Safety
    /// Writes to page tables and PMM state. Single-threaded kernel only.
    unsafe fn alloc(&mut self) -> Option<*mut u8> {
        // ── Find a slab with at least one free slot ───────────────────────────
        let mut slab = self.head;
        while !slab.is_null() {
            let s = &mut *slab;
            if !s.free_list.is_null() {
                // Pop the head of the free list.
                let node = s.free_list;
                s.free_list = (*node).next;
                s.used += 1;
                return Some(node as *mut u8);
            }
            slab = (*slab).next_slab;
        }

        // ── No free slot found — grow by one slab ─────────────────────────────
        self.grow()
    }

    /// Allocates a new 4 KiB slab, maps it into the heap, initialises the
    /// header and free list, then returns the first object.
    ///
    /// # Safety
    /// Allocates a physical frame, writes page table entries.
    unsafe fn grow(&mut self) -> Option<*mut u8> {
        // 1. Allocate a physical frame.
        let phys = pmm::alloc_frame()?;

        // 2. Allocate a virtual address for this slab in the heap.
        let virt = next_heap_virt();

        // 3. Map the physical frame at that virtual address.
        vmm::map_4k(
            vmm::pml4(),
            virt,
            PhysAddr::new(phys),
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        );

        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        unsafe {
            core::arch::asm!("mfence", options(nostack));
        }

        ptr::write_bytes(virt.as_u64() as *mut u8, 0u8, PAGE_SIZE);
        // 5. Write the slab header at byte 0.
        let obj_size = self.object_size;

        // The first object starts right after the header, aligned to obj_size.
        let header_end = core::mem::size_of::<SlabHeader>();
        // Round up to object alignment (obj_size is always a power of two here).
        let first_obj_offset = align_up(header_end, obj_size);

        let capacity = (PAGE_SIZE - first_obj_offset) / obj_size;
        debug_assert!(capacity > 0, "slab object_size too large for one page");

        let header = virt.as_u64() as *mut SlabHeader;
        (*header).next_slab = self.head;
        (*header).free_list = ptr::null_mut();
        (*header).used = 0;
        (*header).capacity = capacity;
        (*header).object_size = obj_size;

        // 6. Build the intrusive free list (link all objects except the first
        //    which we are about to return).
        let base = virt.as_u64() as usize + first_obj_offset;
        let mut prev_node: *mut FreeNode = ptr::null_mut();

        // Build from last to first so that the head of the list is object[1]
        // (object[0] is returned immediately).
        for i in (1..capacity).rev() {
            let obj_ptr = (base + i * obj_size) as *mut FreeNode;
            (*obj_ptr).next = prev_node;
            prev_node = obj_ptr;
        }
        (*header).free_list = prev_node;
        (*header).used = 1; // object[0] is being allocated right now.

        // 7. Prepend this slab to the cache's slab list.
        self.head = header;

        // 8. Return object[0].
        Some(base as *mut u8)
    }

    /// Returns `ptr` back to this cache's free list.
    ///
    /// `ptr` must have been returned by `alloc()` on the same cache.
    /// Double-free is a kernel bug; in debug builds we assert the object was
    /// actually in use.
    ///
    /// # Safety
    /// Writes to slab metadata. Single-threaded kernel only.
    unsafe fn free(&mut self, ptr: *mut u8) {
        // The slab header is at the 4 KiB-aligned start of the same page.
        let slab_base = ptr as usize & !(PAGE_SIZE - 1);
        let header = slab_base as *mut SlabHeader;

        debug_assert!(
            (*header).used > 0,
            "slab::free: double-free detected at {:p}",
            ptr
        );

        // Push onto the free list.
        let node = ptr as *mut FreeNode;
        (*node).next = (*header).free_list;
        (*header).free_list = node;
        (*header).used -= 1;
    }
}

// ---------------------------------------------------------------------------
// Heap virtual address bump allocator
// ---------------------------------------------------------------------------

/// Next available virtual address in the kernel heap.
///
/// Bumped by 4 KiB every time a new slab is mapped.  Never decremented
/// (slab pages are never unmapped individually — they are recycled in place).
static mut HEAP_NEXT: u64 = vmm::KERNEL_HEAP_START;

/// Returns the next heap virtual address and advances the pointer.
///
/// # Safety
/// Accesses `static mut HEAP_NEXT`. Single-threaded kernel only.
unsafe fn next_heap_virt() -> VirtAddr {
    let addr = HEAP_NEXT;
    HEAP_NEXT += PAGE_SIZE as u64;

    debug_assert!(
        HEAP_NEXT <= vmm::KERNEL_HEAP_START + vmm::KERNEL_HEAP_SIZE,
        "VMM: kernel heap exhausted — increase KERNEL_HEAP_SIZE"
    );

    VirtAddr::new(addr)
}

// ---------------------------------------------------------------------------
// Global slab cache table
// ---------------------------------------------------------------------------

/// One cache per size class.
///
/// Initialised at compile time with `SlabCache::new(size)`.
/// Populated with slab pages lazily on first allocation.
static mut CACHES: [SlabCache; NUM_CLASSES] = [
    SlabCache::new(SIZE_CLASSES[0]), // 16 B
    SlabCache::new(SIZE_CLASSES[1]), // 32 B
    SlabCache::new(SIZE_CLASSES[2]), // 64 B
    SlabCache::new(SIZE_CLASSES[3]), // 128 B
    SlabCache::new(SIZE_CLASSES[4]), // 256 B
    SlabCache::new(SIZE_CLASSES[5]), // 512 B
    SlabCache::new(SIZE_CLASSES[6]), // 1 KiB
    SlabCache::new(SIZE_CLASSES[7]), // 2 KiB
    SlabCache::new(SIZE_CLASSES[8]), // 4 KiB
];

// ---------------------------------------------------------------------------
// Size class lookup
// ---------------------------------------------------------------------------

/// Returns the index into `CACHES` / `SIZE_CLASSES` for the given allocation
/// size.
///
/// Returns `None` if `size` is 0 or larger than the biggest size class.
fn class_for(size: usize) -> Option<usize> {
    if size == 0 {
        return None;
    }
    SIZE_CLASSES.iter().position(|&s| s >= size)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocates `size` bytes of kernel heap memory.
///
/// Rounds `size` up to the next size class (16 … 4096 bytes).
/// Returns a non-null, properly aligned pointer, or `None` if OOM or if
/// `size > 4096`.
///
/// For allocations larger than 4 KiB, call `pmm::alloc_frames_contiguous()`
/// directly and map the frames manually.
///
/// # Safety
/// Writes to slab and page table state. Single-threaded kernel only.
pub unsafe fn kmalloc(size: usize) -> Option<*mut u8> {
    let idx = class_for(size)?;
    CACHES[idx].alloc()
}

/// Frees memory previously returned by `kmalloc`.
///
/// `ptr` must not be null and must not be used after this call.
///
/// # Safety
/// Writes to slab metadata. `ptr` must originate from `kmalloc`.
pub unsafe fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return; // Freeing null is a no-op (matches libc convention).
    }

    // Determine the size class from the slab header.
    let slab_base = ptr as usize & !(PAGE_SIZE - 1);
    let header = slab_base as *mut SlabHeader;
    let obj_size = (*header).object_size;

    let idx = class_for(obj_size).expect("kfree: corrupt slab header — invalid object_size");

    CACHES[idx].free(ptr);
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Initialises the slab allocator.
///
/// At this stage there is nothing to do — caches are lazily populated on
/// first allocation.  This function exists so `main.rs` can call it in the
/// explicit initialisation sequence and print a confirmation message.
///
/// Call after `vmm::init()`.
pub fn init() {
    crate::kprintln!("[SLAB] {} size classes: {:?}", NUM_CLASSES, SIZE_CLASSES,);
    crate::kprintln!(
        "[SLAB] heap: {:#x} – {:#x} ({} MiB)",
        vmm::KERNEL_HEAP_START,
        vmm::KERNEL_HEAP_START + vmm::KERNEL_HEAP_SIZE,
        vmm::KERNEL_HEAP_SIZE / (1024 * 1024),
    );
}

// ---------------------------------------------------------------------------
// Alignment utility
// ---------------------------------------------------------------------------

/// Rounds `n` up to the nearest multiple of `align` (must be power of two).
#[inline]
fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}
