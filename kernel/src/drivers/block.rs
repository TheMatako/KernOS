// kernel/src/drivers/block.rs
//
// RAM disk block device — Brick 7, part 3/3.
//
// ── Design ────────────────────────────────────────────────────────────────────
//
// A RAM disk is the simplest possible block device: a region of physical RAM
// that the VFS (Brick 8) sees as a flat array of 512-byte sectors.
//
// This serves two purposes:
//   1. Gives the VFS layer (ext2) a concrete block device to format and mount
//      during early boot without needing real disk hardware.
//   2. Acts as a high-speed volatile disk (the "surutilisation RAM" strategy):
//      data stored here is lost on reboot but accessed at RAM speed (~50 GB/s).
//
// ── Layout ────────────────────────────────────────────────────────────────────
//
//   Physical RAM: [ramdisk_base … ramdisk_base + size)
//   Sector 0: bytes [0, 511]
//   Sector 1: bytes [512, 1023]
//   …
//   Sector N: bytes [N*512, (N+1)*512 - 1]
//
// The RAM is allocated as a contiguous run of 4 KiB frames from the PMM
// (using `pmm::alloc_frames_contiguous`), then accessed via the VMM direct
// physical map (RAM_DIRECT_MAP_BASE + phys_addr).
//
// ── Interface ─────────────────────────────────────────────────────────────────
//
//   block::read_sector (lba, buf)  → reads  512 bytes from sector `lba`
//   block::write_sector(lba, buf)  → writes 512 bytes to  sector `lba`
//
// These are the exact functions the ext2 driver (Brick 8) will call.
//
// ── Geometry ──────────────────────────────────────────────────────────────────
//
// The RAM disk size is configurable at init time (`init(size_bytes)`).
// Default in main.rs: 32 MiB → 65 536 sectors — enough for a small ext2 fs.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use crate::{pmm, vmm};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of one sector in bytes (matches the ext2 / FAT standard block size).
pub const SECTOR_SIZE: usize = 512;

// ---------------------------------------------------------------------------
// Static state
// ---------------------------------------------------------------------------

/// Physical base address of the RAM disk (allocated by PMM).
static mut RAMDISK_PHYS_BASE: u64 = 0;

/// Total size of the RAM disk in bytes.
static mut RAMDISK_SIZE: usize = 0;

/// Total number of sectors in the RAM disk.
static mut SECTOR_COUNT: usize = 0;

/// `true` once `init()` has completed successfully.
static mut INITIALIZED: bool = false;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns a mutable byte slice covering the entire RAM disk.
///
/// The slice is in the VMM direct physical map:
///   virtual address = RAM_DIRECT_MAP_BASE + RAMDISK_PHYS_BASE
///
/// # Safety
/// - `init()` must have been called.
/// - The direct physical map must be active (vmm::init() must have run).
/// - Caller must ensure no concurrent mutable access to the same sector.
#[inline]
unsafe fn ramdisk_slice() -> &'static mut [u8] {
    let virt = vmm::phys_to_virt(x86_64::PhysAddr::new(RAMDISK_PHYS_BASE));
    core::slice::from_raw_parts_mut(virt.as_mut_ptr(), RAMDISK_SIZE)
}

/// Validates that `lba` is within range.
///
/// Returns an error string if out of bounds, or `Ok(())` if valid.
#[inline]
fn check_lba(lba: usize) -> Result<(), &'static str> {
    let sector_count = unsafe { SECTOR_COUNT };
    if lba >= sector_count {
        Err("block: LBA out of range")
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Reads one 512-byte sector from the RAM disk into `buf`.
///
/// # Arguments
/// * `lba` — Logical Block Address (0-indexed sector number).
/// * `buf` — destination buffer, must be exactly `SECTOR_SIZE` (512) bytes.
///
/// # Errors
/// Returns `Err(&str)` if:
///   - `buf.len() != SECTOR_SIZE`
///   - `lba` is out of range
///   - the driver has not been initialised
///
/// # Safety
/// Reads from the VMM direct physical map. `init()` and `vmm::init()` must
/// have been called first.
pub unsafe fn read_sector(lba: usize, buf: &mut [u8]) -> Result<(), &'static str> {
    if !INITIALIZED {
        return Err("block: driver not initialised");
    }
    if buf.len() != SECTOR_SIZE {
        return Err("block: buffer must be exactly SECTOR_SIZE bytes");
    }
    check_lba(lba)?;

    let offset = lba * SECTOR_SIZE;
    let disk = ramdisk_slice();

    // Copy 512 bytes from the RAM disk into the caller's buffer.
    buf.copy_from_slice(&disk[offset..offset + SECTOR_SIZE]);
    Ok(())
}

/// Writes one 512-byte sector from `buf` into the RAM disk.
///
/// # Arguments
/// * `lba` — Logical Block Address (0-indexed sector number).
/// * `buf` — source buffer, must be exactly `SECTOR_SIZE` (512) bytes.
///
/// # Errors
/// Returns `Err(&str)` if:
///   - `buf.len() != SECTOR_SIZE`
///   - `lba` is out of range
///   - the driver has not been initialised
///
/// # Safety
/// Writes to the VMM direct physical map. `init()` and `vmm::init()` must
/// have been called first.
pub unsafe fn write_sector(lba: usize, buf: &[u8]) -> Result<(), &'static str> {
    if !INITIALIZED {
        return Err("block: driver not initialised");
    }
    if buf.len() != SECTOR_SIZE {
        return Err("block: buffer must be exactly SECTOR_SIZE bytes");
    }
    check_lba(lba)?;

    let offset = lba * SECTOR_SIZE;
    let disk = ramdisk_slice();

    // Copy 512 bytes from the caller's buffer into the RAM disk.
    disk[offset..offset + SECTOR_SIZE].copy_from_slice(buf);
    Ok(())
}

/// Returns the total number of sectors in the RAM disk.
pub fn sector_count() -> usize {
    unsafe { SECTOR_COUNT }
}

/// Returns the total size of the RAM disk in bytes.
pub fn size_bytes() -> usize {
    unsafe { RAMDISK_SIZE }
}

/// Zeroes every sector of the RAM disk.
///
/// Call this before formatting the disk with ext2 (Brick 8).
///
/// # Safety
/// Writes to the VMM direct physical map.
pub unsafe fn zero_all() {
    if !INITIALIZED {
        return;
    }
    let disk = ramdisk_slice();
    disk.fill(0);
    crate::kprintln!(
        "[BLOCK] RAM disk zeroed ({} MiB)",
        RAMDISK_SIZE / (1024 * 1024)
    );
}

/// Fills every sector with a recognisable test pattern (0xDE 0xAD 0xBE 0xEF …).
///
/// Useful for detecting unwritten sectors during VFS development.
///
/// # Safety
/// Writes to the VMM direct physical map.
pub unsafe fn fill_test_pattern() {
    if !INITIALIZED {
        return;
    }
    let disk = ramdisk_slice();
    for (i, byte) in disk.iter_mut().enumerate() {
        *byte = [0xDE_u8, 0xAD, 0xBE, 0xEF][i % 4];
    }
    crate::kprintln!("[BLOCK] RAM disk filled with test pattern");
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Allocates a contiguous RAM region and registers it as the block device.
///
/// # Arguments
/// * `size_bytes` — desired RAM disk size (rounded up to the nearest 4 KiB).
///   Pass `32 * 1024 * 1024` (32 MiB) for a comfortable ext2 fs.
///
/// # Safety
/// - Calls `pmm::alloc_frames_contiguous` — PMM must be initialised.
/// - Accesses `vmm::phys_to_virt` — VMM direct map must be active.
/// - Writes to `static mut` globals.
/// - Must be called once, with interrupts disabled.
pub unsafe fn init(size_bytes: usize) {
    // Round up to the next 4 KiB boundary so we allocate whole frames.
    let frame_size = pmm::FRAME_SIZE as usize;
    let size_aligned = (size_bytes + frame_size - 1) & !(frame_size - 1);
    let n_frames = size_aligned / frame_size;

    // Allocate a contiguous physical region from the PMM.
    let phys_base =
        pmm::alloc_frames_contiguous(n_frames).expect("block: PMM OOM allocating RAM disk");

    RAMDISK_PHYS_BASE = phys_base;
    RAMDISK_SIZE = size_aligned;
    SECTOR_COUNT = size_aligned / SECTOR_SIZE;
    INITIALIZED = true;

    // Zero the disk so it looks like a blank device (no garbage sectors).
    zero_all();

    crate::kprintln!(
        "[BLOCK] RAM disk: {} MiB  |  {} sectors  |  phys={:#x}  virt={:#x}",
        size_aligned / (1024 * 1024),
        SECTOR_COUNT,
        phys_base,
        vmm::phys_to_virt(x86_64::PhysAddr::new(phys_base)).as_u64(),
    );
}

// ---------------------------------------------------------------------------
// Smoke test
// ---------------------------------------------------------------------------

/// Writes a pattern to sector 0, reads it back, and verifies correctness.
///
/// Called from `main.rs` to validate the block driver before the VFS uses it.
///
/// # Safety
/// Calls `read_sector` / `write_sector`.
pub unsafe fn smoke_test() {
    // Write a known pattern.
    let mut write_buf = [0u8; SECTOR_SIZE];
    for (i, b) in write_buf.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }

    write_sector(0, &write_buf).expect("block smoke_test: write failed");

    // Read it back.
    let mut read_buf = [0u8; SECTOR_SIZE];
    read_sector(0, &mut read_buf).expect("block smoke_test: read failed");

    // Verify byte by byte.
    for (i, (&w, &r)) in write_buf.iter().zip(read_buf.iter()).enumerate() {
        assert_eq!(w, r, "block smoke_test: mismatch at byte {}", i);
    }

    crate::kprintln!("[BLOCK] smoke test passed — sector 0 write/read OK");
}
