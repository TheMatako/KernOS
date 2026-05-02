// kernel/src/pmm.rs
//
// Physical Memory Manager (PMM) — Brick 3.
//
// ── Philosophy: aggressive RAM usage ─────────────────────────────────────────
//
// KernOS is designed to *use* RAM rather than sit on it.  The PMM exposes
// three levels of allocation to support this goal:
//
//   1. `alloc_frame()`            → one 4 KiB frame   (fine-grained)
//   2. `alloc_huge_frame()`       → one 2 MiB frame   (huge page, always aligned)
//   3. `alloc_frames_contiguous(n)`→ n contiguous 4 KiB frames (RAM disk, DMA, slab)
//
// The bitmap tracks everything at 4 KiB granularity.  A 2 MiB huge page is
// simply 512 consecutive 4 KiB frames all allocated at once.
//
// ── Bitmap design ─────────────────────────────────────────────────────────────
//
// One bit per 4 KiB frame:   0 = FREE,  1 = USED.
//
// Supported physical range: 64 GiB → 16 777 216 frames → 2 MiB bitmap.
// The bitmap lives in .bss (zeroed before init()) and is immediately
// overwritten by init() with a pessimistic "all USED" baseline; only
// MemoryRegionKind::Usable regions are flipped to FREE.
//
// ── Initialisation policy ────────────────────────────────────────────────────
//
//   Usable                 → FREE
//   KernelCode             → USED  (our own code/data — never touch)
//   Reserved               → USED  (MMIO, ACPI tables, firmware, …)
//   UefiRuntime            → USED  (runtime services still alive)
//   BootloaderReclaimable  → USED  (safe to free once we no longer need BootInfo)
//   everything else        → USED  (safe default for unknown kinds)
//
// ── Next-fit hint ─────────────────────────────────────────────────────────────
//
// `NEXT_SEARCH` tracks the last allocation site.  On the next call we start
// scanning from there, wrapping around once.  This keeps the allocator O(N)
// worst-case but avoids constant rescanning of the low-memory region which is
// densely occupied by kernel code and hardware reservations.

#![allow(dead_code)]

use shared::BootInfo;
use shared::{MemoryMap, MemoryRegionKind};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum supported physical memory: 64 GiB.
pub const MAX_PHYS_MEMORY: u64 = 64 * 1024 * 1024 * 1024;

/// Size of a standard 4 KiB frame.
pub const FRAME_SIZE: u64 = 4 * 1024;

/// Size of a 2 MiB huge page (512 × 4 KiB frames).
pub const HUGE_FRAME_SIZE: u64 = 2 * 1024 * 1024;

/// Number of 4 KiB frames per 2 MiB huge page.
pub const FRAMES_PER_HUGE: usize = (HUGE_FRAME_SIZE / FRAME_SIZE) as usize; // 512

/// Total number of 4 KiB frames we track.
const TOTAL_FRAMES: usize = (MAX_PHYS_MEMORY / FRAME_SIZE) as usize; // 16 777 216

/// Size of the bitmap in bytes (one bit per frame).
const BITMAP_BYTES: usize = TOTAL_FRAMES / 8; // 2 097 152 (2 MiB)

// ---------------------------------------------------------------------------
// Static state — all in .bss, zeroed by zero_bss() before init()
// ---------------------------------------------------------------------------

/// The allocation bitmap.
///
/// bit 0 = FREE, bit 1 = USED.
/// Index: frame N → byte (N/8), bit (N%8).
///
/// 2 MiB of .bss — this is intentional.  RAM is meant to be used, and having
/// a flat bitmap means O(1) address↔frame conversions with no pointer chasing.
static mut BITMAP: [u8; BITMAP_BYTES] = [0u8; BITMAP_BYTES];

/// Next-fit search cursor (byte index into BITMAP).
///
/// Avoids rescanning from 0 on every allocation.
static mut NEXT_SEARCH: usize = 0;

/// Number of currently free 4 KiB frames.
static mut FREE_FRAME_COUNT: usize = 0;

/// Total usable 4 KiB frames discovered from the memory map.
///
/// Stored separately from FREE_FRAME_COUNT so we can report how many frames
/// have been consumed even after partial frees.
static mut TOTAL_USABLE_FRAMES: usize = 0;

// ---------------------------------------------------------------------------
// Bitmap helpers (private)
// ---------------------------------------------------------------------------

/// Returns `(byte_index, bit_mask)` for the given frame index.
#[inline]
fn bitmap_pos(frame: usize) -> (usize, u8) {
    (frame / 8, 1u8 << (frame % 8))
}

/// Returns `true` if `frame` is marked USED.
///
/// # Safety
/// Reads `static mut BITMAP`. Single-threaded kernel only.
#[inline]
unsafe fn is_used(frame: usize) -> bool {
    let (byte, bit) = bitmap_pos(frame);
    BITMAP[byte] & bit != 0
}

/// Marks `frame` as USED (bit ← 1).
///
/// # Safety
/// Writes `static mut BITMAP`. Single-threaded kernel only.
#[inline]
unsafe fn mark_used(frame: usize) {
    let (byte, bit) = bitmap_pos(frame);
    BITMAP[byte] |= bit;
}

/// Marks `frame` as FREE (bit ← 0).
///
/// # Safety
/// Writes `static mut BITMAP`. Single-threaded kernel only.
#[inline]
unsafe fn mark_free(frame: usize) {
    let (byte, bit) = bitmap_pos(frame);
    BITMAP[byte] &= !bit;
}

// ---------------------------------------------------------------------------
// Address ↔ frame index conversions (public)
// ---------------------------------------------------------------------------

/// Converts a physical address to its 4 KiB frame index.
///
/// The address is rounded *down* to the nearest frame boundary.
///
/// # Panics
/// Panics if `addr` is beyond `MAX_PHYS_MEMORY`.
#[inline]
pub fn addr_to_frame(addr: u64) -> usize {
    assert!(
        addr < MAX_PHYS_MEMORY,
        "PMM: addr {:#x} exceeds MAX_PHYS_MEMORY",
        addr
    );
    (addr / FRAME_SIZE) as usize
}

/// Converts a 4 KiB frame index back to its base physical address.
#[inline]
pub fn frame_to_addr(frame: usize) -> u64 {
    frame as u64 * FRAME_SIZE
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Initialises the PMM from the bootloader memory map.
///
/// Call order in `kernel_main`:
///   1. `zero_bss()`
///   2. `serial::init()`
///   3. `gdt::init()`
///   4. `idt::init()`
///   5. **`pmm::init(&boot_info.memory_map)`**
///
/// # Safety
/// Writes to `static mut` globals. Must be called exactly once.
/// Initialises the PMM from the bootloader memory map.
pub unsafe fn init(boot_info: &BootInfo) {
    // ── Step 1 — Mark everything as USED (pessimistic baseline) ───────────────
    #[allow(static_mut_refs)]
    for byte in BITMAP.iter_mut() {
        *byte = 0xFF;
    }

    // ── NOUVEAU : Extraire les zones critiques à protéger ─────────────────────
    let fb_start = boot_info.framebuffer.map(|f| f.base).unwrap_or(0);
    let fb_end = boot_info
        .framebuffer
        .map(|f| f.base + f.size as u64)
        .unwrap_or(0);

    let kernel_start = boot_info.kernel_physical_start;
    let kernel_end = kernel_start + boot_info.kernel_size;

    // ── Step 2 — Scan the memory map and free Usable regions ──────────────────
    let mut usable_frames: usize = 0;

    for region in boot_info.memory_map.valid_entries() {
        match region.kind {
            MemoryRegionKind::Usable => {
                let start = align_up(region.base, FRAME_SIZE);
                let end = align_down(region.base + region.length, FRAME_SIZE);

                if start >= end {
                    continue;
                }

                let first_frame = addr_to_frame(start);
                let last_frame = addr_to_frame(end - 1);

                for frame in first_frame..=last_frame {
                    let phys_addr = frame_to_addr(frame);

                    // --- LA MAGIE EST ICI ---
                    // 1. On interdit au PMM de donner la mémoire de l'écran (même si le BIOS dit qu'elle est libre)
                    if phys_addr >= fb_start && phys_addr < fb_end {
                        continue;
                    }
                    // 2. On interdit de toucher au kernel lui-même
                    if phys_addr >= kernel_start && phys_addr < kernel_end {
                        continue;
                    }
                    // ------------------------

                    mark_free(frame);
                    usable_frames += 1;
                }
            }

            MemoryRegionKind::BootloaderReclaimable => {}
            _ => {}
        }
    }

    FREE_FRAME_COUNT = usable_frames;
    TOTAL_USABLE_FRAMES = usable_frames;

    crate::kprintln!(
        "[PMM] init complete — {} MiB usable  ({} × 4 KiB frames)",
        (usable_frames as u64 * FRAME_SIZE) / (1024 * 1024),
        usable_frames,
    );
    crate::kprintln!(
        "[PMM] bitmap: {} KiB  |  max physical: {} GiB",
        BITMAP_BYTES / 1024,
        MAX_PHYS_MEMORY / (1024 * 1024 * 1024),
    );
}

// ---------------------------------------------------------------------------
// Core allocator — single 4 KiB frame
// ---------------------------------------------------------------------------

/// Allocates one free 4 KiB physical frame.
///
/// Returns the **physical base address** of the frame (always 4 KiB-aligned),
/// or `None` if physical memory is exhausted.
///
/// Uses a next-fit strategy: scanning restarts from where the previous
/// allocation succeeded, wrapping around once before giving up.
///
/// # Safety
/// Writes to `static mut` globals. Single-threaded kernel only.
pub unsafe fn alloc_frame() -> Option<u64> {
    // We scan bytes rather than bits for speed: if a byte is 0xFF, all 8
    // frames in it are used and we skip it in one compare.

    let start_byte = NEXT_SEARCH;

    // First pass: from NEXT_SEARCH to end of bitmap.
    if let Some(addr) = scan_range(start_byte, BITMAP_BYTES) {
        return Some(addr);
    }

    // Second pass: wrap around from 0 to start_byte.
    if let Some(addr) = scan_range(0, start_byte) {
        return Some(addr);
    }

    // Out of memory.
    None
}

/// Inner scan helper: searches `BITMAP[byte_start..byte_end]` for a free bit.
///
/// Returns the physical address of the allocated frame, or `None`.
///
/// # Safety
/// Reads/writes `static mut BITMAP`, `NEXT_SEARCH`, `FREE_FRAME_COUNT`.
#[allow(clippy::needless_range_loop)]
unsafe fn scan_range(byte_start: usize, byte_end: usize) -> Option<u64> {
    for byte_idx in byte_start..byte_end {
        // Fast skip: all 8 frames in this byte are used.
        if BITMAP[byte_idx] == 0xFF {
            continue;
        }

        // At least one free frame in this byte — find which bit.
        for bit in 0..8u8 {
            let frame = byte_idx * 8 + bit as usize;

            if frame >= TOTAL_FRAMES {
                return None;
            }

            if !is_used(frame) {
                mark_used(frame);
                FREE_FRAME_COUNT -= 1;
                // Update the hint to the *next* byte so subsequent calls
                // continue forward from here.
                NEXT_SEARCH = byte_idx;
                return Some(frame_to_addr(frame));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Core allocator — 2 MiB huge frame
// ---------------------------------------------------------------------------

/// Allocates one 2 MiB huge page (512 contiguous 4 KiB frames, 2 MiB-aligned).
///
/// Returns the physical base address (always 2 MiB-aligned), or `None`.
///
/// Huge pages are used for large kernel data structures (PMM bitmap mirror,
/// RAM disk, slab caches) to reduce TLB pressure.  The VMM (Brick 4) maps
/// them with the PS (Page Size) bit in the PD entry.
///
/// # Safety
/// Writes to `static mut` globals. Single-threaded kernel only.
pub unsafe fn alloc_huge_frame() -> Option<u64> {
    // Huge pages must start at a 2 MiB boundary → frame index divisible by 512.
    // We scan in steps of 512 frames (= 64 bytes in the bitmap).
    let step = FRAMES_PER_HUGE; // 512

    let mut frame = 0usize;
    while frame + step <= TOTAL_FRAMES {
        if is_huge_block_free(frame) {
            // Mark all 512 frames as used.
            for f in frame..frame + step {
                mark_used(f);
            }
            FREE_FRAME_COUNT = FREE_FRAME_COUNT.saturating_sub(step);
            return Some(frame_to_addr(frame));
        }
        frame += step;
    }
    None
}

/// Returns `true` if all 512 frames starting at `first_frame` are free.
///
/// # Safety
/// Reads `static mut BITMAP`. Single-threaded kernel only.
#[allow(clippy::needless_range_loop)]
unsafe fn is_huge_block_free(first_frame: usize) -> bool {
    // Check 64 consecutive bytes (= 512 bits = 512 frames).
    // If any byte is non-zero, at least one frame is used.
    let first_byte = first_frame / 8;
    for byte_idx in first_byte..first_byte + (FRAMES_PER_HUGE / 8) {
        if BITMAP[byte_idx] != 0x00 {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Bulk allocator — contiguous 4 KiB frames (RAM disk, DMA, slab)
// ---------------------------------------------------------------------------

/// Allocates `count` contiguous 4 KiB frames.
///
/// Returns the physical base address of the first frame, or `None` if no
/// contiguous run of `count` free frames could be found.
///
/// This is used by:
///   - The RAM disk (Brick 7): allocate a large contiguous region once.
///   - The slab allocator (Brick 4): grab a slab of pages at a time.
///   - DMA buffers (future): hardware requires physically contiguous memory.
///
/// # Safety
/// Writes to `static mut` globals. Single-threaded kernel only.
pub unsafe fn alloc_frames_contiguous(count: usize) -> Option<u64> {
    if count == 0 {
        return None;
    }

    let mut run_start = 0usize;
    let mut run_len = 0usize;

    for frame in 0..TOTAL_FRAMES {
        if !is_used(frame) {
            if run_len == 0 {
                run_start = frame;
            }
            run_len += 1;

            if run_len == count {
                // Found a long enough run — mark all frames as used.
                for f in run_start..run_start + count {
                    mark_used(f);
                }
                FREE_FRAME_COUNT = FREE_FRAME_COUNT.saturating_sub(count);
                return Some(frame_to_addr(run_start));
            }
        } else {
            // Run broken — reset.
            run_len = 0;
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Deallocation
// ---------------------------------------------------------------------------

/// Frees a previously allocated 4 KiB frame.
///
/// `addr` must be the exact address returned by `alloc_frame()` or
/// `alloc_frames_contiguous()`.  Double-free is a kernel bug and is caught
/// by the debug assertion below.
///
/// # Safety
/// Writes to `static mut` globals. Single-threaded kernel only.
pub unsafe fn free_frame(addr: u64) {
    debug_assert!(
        addr.is_multiple_of(FRAME_SIZE),
        "PMM: free_frame called with non-aligned address {:#x}",
        addr
    );

    let frame = addr_to_frame(addr);

    debug_assert!(
        is_used(frame),
        "PMM: double-free detected for frame {:#x}",
        addr
    );

    mark_free(frame);
    FREE_FRAME_COUNT += 1;

    // Update next-fit hint: if this free frame is before the current cursor,
    // move the cursor back so future allocations can reuse it immediately.
    let byte = frame / 8;
    if byte < NEXT_SEARCH {
        NEXT_SEARCH = byte;
    }
}

/// Frees `count` contiguous frames starting at `base_addr`.
///
/// Convenience wrapper around `free_frame()` for bulk frees (RAM disk,
/// slab deallocation, etc.).
///
/// # Safety
/// Same as `free_frame()`.
pub unsafe fn free_frames_contiguous(base_addr: u64, count: usize) {
    for i in 0..count as u64 {
        free_frame(base_addr + i * FRAME_SIZE);
    }
}

/// Frees a 2 MiB huge page (512 frames) previously allocated by
/// `alloc_huge_frame()`.
///
/// # Safety
/// Same as `free_frame()`.
pub unsafe fn free_huge_frame(addr: u64) {
    debug_assert!(
        addr.is_multiple_of(HUGE_FRAME_SIZE),
        "PMM: free_huge_frame called with non-2MiB-aligned address {:#x}",
        addr
    );
    free_frames_contiguous(addr, FRAMES_PER_HUGE);
}

// ---------------------------------------------------------------------------
// Bootloader memory reclamation
// ---------------------------------------------------------------------------

/// Frees all `BootloaderReclaimable` regions so the kernel can use them.
///
/// Call this **only after** you have finished reading `BootInfo` and copied
/// everything you need into your own kernel structures.  After this call,
/// the memory previously occupied by the bootloader image and stack becomes
/// available as ordinary free frames.
///
/// # Safety
/// Writes to `static mut` globals.  Must be called after Brick 4 (VMM)
/// has built its own page tables — accessing BootInfo after this is UB.
#[allow(dead_code)]
pub unsafe fn reclaim_bootloader_memory(memory_map: &MemoryMap) {
    let mut reclaimed: usize = 0;

    for region in memory_map.valid_entries() {
        if region.kind == MemoryRegionKind::BootloaderReclaimable {
            let start = align_up(region.base, FRAME_SIZE);
            let end = align_down(region.base + region.length, FRAME_SIZE);

            if start >= end {
                continue;
            }

            let first_frame = addr_to_frame(start);
            let last_frame = addr_to_frame(end - 1);

            for frame in first_frame..=last_frame {
                if is_used(frame) {
                    mark_free(frame);
                    FREE_FRAME_COUNT += 1;
                    TOTAL_USABLE_FRAMES += 1;
                    reclaimed += 1;
                }
            }
        }
    }

    crate::kprintln!(
        "[PMM] bootloader memory reclaimed: {} KiB ({} frames)",
        (reclaimed as u64 * FRAME_SIZE) / 1024,
        reclaimed,
    );
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Returns the number of free 4 KiB frames.
///
/// Multiply by `FRAME_SIZE` to get bytes.
pub fn free_frames() -> usize {
    // Safety: read-only access to a usize (atomic on x86_64, single-threaded).
    unsafe { FREE_FRAME_COUNT }
}

/// Returns the total number of usable 4 KiB frames discovered at boot.
pub fn total_usable_frames() -> usize {
    unsafe { TOTAL_USABLE_FRAMES }
}

/// Returns the number of currently allocated frames.
pub fn used_frames() -> usize {
    unsafe { TOTAL_USABLE_FRAMES - FREE_FRAME_COUNT }
}

/// Prints a RAM usage summary to the serial port.
pub fn print_stats() {
    let free = free_frames();
    let total = total_usable_frames();
    let used = used_frames();

    crate::kprintln!("[PMM] ── RAM statistics ──────────────────────");
    crate::kprintln!(
        "[PMM]   Total usable : {:6} MiB  ({} frames)",
        (total as u64 * FRAME_SIZE) / (1024 * 1024),
        total
    );
    crate::kprintln!(
        "[PMM]   Used         : {:6} MiB  ({} frames)",
        (used as u64 * FRAME_SIZE) / (1024 * 1024),
        used
    );
    crate::kprintln!(
        "[PMM]   Free         : {:6} MiB  ({} frames)",
        (free as u64 * FRAME_SIZE) / (1024 * 1024),
        free
    );
    crate::kprintln!("[PMM] ───────────────────────────────────────");
}

// ---------------------------------------------------------------------------
// Alignment utilities (private)
// ---------------------------------------------------------------------------

/// Rounds `addr` up to the nearest multiple of `align`.
/// `align` must be a power of two.
#[inline]
fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// Rounds `addr` down to the nearest multiple of `align`.
/// `align` must be a power of two.
#[inline]
fn align_down(addr: u64, align: u64) -> u64 {
    addr & !(align - 1)
}
