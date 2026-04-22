// ============================================================
// KernOS — Shared Types
// ============================================================
//
// This crate defines types shared between the bootloader and
// the kernel. It has no dependencies and works in no_std.
//
// The bootloader fills BootInfo before jumping to kernel_main.
// The kernel receives a pointer to it as its first argument.
// ============================================================

#![no_std]

// ------------------------------------------------------------
// Constants
// ------------------------------------------------------------

/// Maximum number of memory map entries we support.
/// UEFI typically returns 30-60 entries on a real machine.
/// 256 is a safe upper bound that fits in a fixed-size array.
/// We use a fixed array because Vec requires a heap allocator
/// which does not exist yet at boot time.
pub const MAX_MEMORY_MAP_ENTRIES: usize = 256;

// ------------------------------------------------------------
// Memory map types
// ------------------------------------------------------------

/// Describes what a memory zone is used for.
/// These types come from the UEFI specification.
/// The kernel uses them to know which zones it can use freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MemoryRegionKind {
    /// Free RAM — the kernel can use this zone however it wants.
    Usable = 0,

    /// Used by the bootloader code and data.
    /// The kernel can reclaim this zone after boot is complete.
    BootloaderReclaimable = 1,

    /// Used by the UEFI firmware runtime services.
    /// The kernel must not touch this zone.
    UefiRuntime = 2,

    /// Reserved by the hardware or firmware.
    /// The kernel must not touch this zone.
    Reserved = 3,

    /// Contains the kernel itself (.text, .data, .bss sections).
    /// The kernel must not overwrite this zone.
    KernelCode = 4,

    /// Unknown or other UEFI memory type — treat as reserved.
    Unknown = 5,
}

/// Describes one contiguous zone of physical memory.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MemoryRegion {
    /// Physical start address of this memory zone.
    pub base: u64,

    /// Size of this zone in bytes.
    pub length: u64,

    /// What this zone is used for.
    pub kind: MemoryRegionKind,
}

/// The full physical memory map passed from bootloader to kernel.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct MemoryMap {
    /// All memory regions. Only the first entry_count are valid.
    pub entries: [MemoryRegion; MAX_MEMORY_MAP_ENTRIES],

    /// Number of valid entries in the array.
    pub entry_count: usize,
}

impl MemoryMap {
    /// Creates an empty memory map with no valid entries.
    pub const fn new() -> Self {
        Self {
            entries: [MemoryRegion {
                base: 0,
                length: 0,
                kind: MemoryRegionKind::Unknown,
            }; MAX_MEMORY_MAP_ENTRIES],
            entry_count: 0,
        }
    }

    /// Returns only the valid entries as a slice.
    pub fn valid_entries(&self) -> &[MemoryRegion] {
        &self.entries[..self.entry_count]
    }

    /// Adds a new entry to the memory map.
    /// Does nothing if the map is already full.
    pub fn add_entry(&mut self, region: MemoryRegion) {
        if self.entry_count < MAX_MEMORY_MAP_ENTRIES {
            self.entries[self.entry_count] = region;
            self.entry_count += 1;
        }
    }
}

// ------------------------------------------------------------
// Framebuffer
// ------------------------------------------------------------

/// Information about the UEFI GOP framebuffer.
/// Writing pixels to `base` directly draws to the screen.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FramebufferInfo {
    /// Physical address of the framebuffer memory.
    pub base: u64,

    /// Total size of the framebuffer in bytes.
    pub size: usize,

    /// Width of the screen in pixels.
    pub width: u32,

    /// Height of the screen in pixels.
    pub height: u32,

    /// Bytes per row — may be larger than width * bytes_per_pixel
    /// due to alignment padding added by the GPU.
    pub stride: u32,

    /// Bytes per pixel — typically 4 (32-bit BGRX color).
    pub bytes_per_pixel: u32,
}

// ------------------------------------------------------------
// BootInfo — the main structure passed to the kernel
// ------------------------------------------------------------

/// All information the bootloader passes to the kernel.
///
/// The bootloader fills this structure, places it in memory,
/// then passes a pointer to it as the first argument of kernel_main().
///
/// repr(C) guarantees identical memory layout in both the
/// bootloader and the kernel, regardless of compiler settings.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct BootInfo {
    /// Physical memory map — which zones are free, reserved, etc.
    /// The PMM (Brick 3) uses this to initialize frame allocation.
    pub memory_map: MemoryMap,

    /// Screen framebuffer — where and how to draw pixels.
    /// None if no GOP framebuffer is available.
    pub framebuffer: Option<FramebufferInfo>,

    /// Physical address where the kernel ELF was loaded.
    /// The PMM uses this to mark kernel memory as non-free.
    pub kernel_physical_start: u64,

    /// Size of the kernel image in bytes.
    pub kernel_size: u64,

    /// Physical address of the ACPI RSDP table.
    /// None if ACPI is not available on this machine.
    pub rsdp_address: Option<u64>,
}

impl BootInfo {
    /// Creates a BootInfo with safe default values.
    pub const fn new() -> Self {
        Self {
            memory_map: MemoryMap::new(),
            framebuffer: None,
            kernel_physical_start: 0,
            kernel_size: 0,
            rsdp_address: None,
        }
    }
}