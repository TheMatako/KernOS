// kernel/src/vmm.rs
//
// Virtual Memory Manager (VMM) — Brick 4, part 1/2.
//
// ── x86_64 four-level paging recap ───────────────────────────────────────────
//
// A 64-bit virtual address is split into five fields:
//
//   63      48 47    39 38    30 29    21 20    12 11       0
//   ┌─────────┬────────┬────────┬────────┬────────┬──────────┐
//   │ (sign)  │  PML4  │  PDPT  │   PD   │   PT   │  offset  │
//   │ ignored │  9 bit │  9 bit │  9 bit │  9 bit │  12 bit  │
//   └─────────┴────────┴────────┴────────┴────────┴──────────┘
//
// Each level is a 4 KiB table of 512 × 8-byte entries.
//
// A 2 MiB huge page short-circuits the last level:
//   PML4 → PDPT → PD entry with PS=1  →  2 MiB physical frame
//
// ── Our virtual memory map ───────────────────────────────────────────────────
//
//   0x0000_0000_0000_0000 – 0x0000_0000_001F_FFFF  : first 2 MiB (identity)
//   0x0000_0000_0010_0000 – …                       : kernel code (1 MiB phys)
//   KERNEL_HEAP_START – KERNEL_HEAP_START + HEAP_SIZE : kernel heap (slab)
//   RAM_DIRECT_MAP_BASE  – +max_phys_addr            : direct physical map
//                                                      (huge pages, 2 MiB each)
//
// The direct physical map lets the kernel address any physical frame by:
//   virtual = RAM_DIRECT_MAP_BASE + physical_address
//
// This is the same approach used by Linux (PAGE_OFFSET) and makes the PMM
// and slab allocator trivial: no separate "virtual allocation" step needed.
//
// ── Safety model ─────────────────────────────────────────────────────────────
//
// All functions that touch CR3 or write page table entries are `unsafe`.
// We use the x86_64 crate's type-safe wrappers wherever possible:
//   `PhysAddr`, `VirtAddr`      — prevent mixing up physical and virtual.
//   `PageTableFlags`             — named bitfield instead of raw u64.
//   `Cr3::read()` / `Cr3::write()` — typed access to control registers.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use x86_64::{
    registers::control::{Cr3, Cr3Flags},
    registers::model_specific::{Efer, EferFlags},
    structures::paging::{
        page_table::PageTableEntry, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    },
    PhysAddr, VirtAddr,
};

use crate::pmm;

// ---------------------------------------------------------------------------
// Virtual memory layout constants
// ---------------------------------------------------------------------------

/// Start of the direct physical memory map in virtual address space.
///
/// Every physical address `p` is accessible at `RAM_DIRECT_MAP_BASE + p`.
/// We use the same convention as Linux (0xFFFF_8800_0000_0000).
/// This lies in the "canonical high" half of the 64-bit address space.
pub const RAM_DIRECT_MAP_BASE: u64 = 0xFFFF_8800_0000_0000;

/// Start of the kernel heap in virtual address space.
///
/// Placed right after the direct map to keep things predictable.
/// The slab allocator (slab.rs) carves this range into fixed-size caches.
pub const KERNEL_HEAP_START: u64 = 0xFFFF_C000_0000_0000;

/// Size of the kernel heap region: 512 MiB.
///
/// The slab allocator maps pages here on demand using `pmm::alloc_frame()`.
/// 512 MiB is generous for a kernel heap; increase later if needed.
pub const KERNEL_HEAP_SIZE: u64 = 512 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Static page table storage
// ---------------------------------------------------------------------------

/// The kernel's PML4 table (the root of the page table tree).
///
/// Aligned to 4 KiB as required by the CPU (CR3 must point to a 4 KiB-aligned
/// address).  Lives in .bss → zeroed by zero_bss() before vmm::init().
#[repr(align(4096))]
struct AlignedPageTable(PageTable);

static mut PML4: AlignedPageTable = AlignedPageTable(PageTable::new());

static mut VMM_ACTIVE: bool = false;

unsafe fn get_table_ptr(phys: PhysAddr) -> *mut u8 {
    if VMM_ACTIVE {
        phys_to_virt(phys).as_mut_ptr()
    } else {
        phys.as_u64() as *mut u8
    }
}

// ---------------------------------------------------------------------------
// Internal: physical address of a page table we own
// ---------------------------------------------------------------------------

/// Converts a pointer to one of our statically-allocated page tables into a
/// `PhysAddr`.
///
/// During early boot, the UEFI identity mapping is still active, so virtual
/// address == physical address for kernel statics.  Once we switch to our own
/// CR3, the direct map is live and we use `phys_to_virt` / `virt_to_phys`.
///
/// # Safety
/// Only valid before we remap the kernel itself.  After that, use the direct
/// map offset.
unsafe fn static_virt_to_phys(ptr: *const PageTable) -> PhysAddr {
    // During early boot: identity-mapped → virt == phys.
    PhysAddr::new(ptr as u64)
}

// ---------------------------------------------------------------------------
// Direct map helpers (public)
// ---------------------------------------------------------------------------

/// Converts a physical address to its virtual address in the direct map.
///
/// Valid after `vmm::init()` has built and loaded the direct map.
#[inline]
pub fn phys_to_virt(phys: PhysAddr) -> VirtAddr {
    VirtAddr::new(RAM_DIRECT_MAP_BASE + phys.as_u64())
}

/// Converts a virtual address (in the direct map) back to a physical address.
///
/// # Panics
/// Panics if `virt` is below `RAM_DIRECT_MAP_BASE`.
#[inline]
pub fn virt_to_phys(virt: VirtAddr) -> PhysAddr {
    assert!(
        virt.as_u64() >= RAM_DIRECT_MAP_BASE,
        "VMM: virt_to_phys called on non-direct-map address {:#x}",
        virt.as_u64()
    );
    PhysAddr::new(virt.as_u64() - RAM_DIRECT_MAP_BASE)
}

// ---------------------------------------------------------------------------
// Page table walker
// ---------------------------------------------------------------------------

/// Allocates a fresh 4 KiB physical frame and zeroes it.
///
/// Used when we need to create a new intermediate page table (PDPT, PD, PT).
///
/// # Safety
/// Calls `pmm::alloc_frame()`. Returns a physical address.
unsafe fn alloc_table() -> PhysAddr {
    let phys = pmm::alloc_frame().expect("VMM: out of physical memory");
    let virt = get_table_ptr(PhysAddr::new(phys)) as *mut u64; // On travaille en u64 (8 octets)

    for i in 0..512 {
        core::ptr::write_volatile(virt.add(i), 0);
    }

    PhysAddr::new(phys)
}

/// Returns a mutable reference to the `PageTable` located at `phys`.
///
/// During early boot: phys == virt (identity map still active).
/// After init: use phys_to_virt.
///
/// # Safety
/// `phys` must point to a valid, properly aligned 4 KiB page table frame.
unsafe fn table_at(phys: PhysAddr) -> &'static mut PageTable {
    &mut *(get_table_ptr(phys) as *mut PageTable) // Traduction sécurisée
}

// ---------------------------------------------------------------------------
// Core mapping function
// ---------------------------------------------------------------------------

/// Maps a single 4 KiB virtual page to a physical frame with given flags.
///
/// Creates intermediate tables (PDPT, PD, PT) as needed.
///
/// # Arguments
/// * `pml4`  — mutable reference to the root page table.
/// * `virt`  — virtual address to map (must be 4 KiB-aligned).
/// * `phys`  — physical address of the frame (must be 4 KiB-aligned).
/// * `flags` — page attributes (PRESENT | WRITABLE | …).
///
/// # Safety
/// - Writes page table entries.
/// - `virt` and `phys` must be 4 KiB-aligned.
/// - Caller must flush the TLB if the mapping replaces an existing one.
pub unsafe fn map_4k(pml4: &mut PageTable, virt: VirtAddr, phys: PhysAddr, flags: PageTableFlags) {
    assert!(
        phys.as_u64().is_multiple_of(4096),
        "VMM: PhysAddr {:#x} non alignée sur 4K",
        phys.as_u64()
    );
    // ── Decompose the virtual address into table indices ─────────────────────
    // VirtAddr::p4_index() etc. give us the 9-bit index at each level.
    let p4_idx = virt.p4_index(); // bits [47:39]
    let p3_idx = virt.p3_index(); // bits [38:30]
    let p2_idx = virt.p2_index(); // bits [29:21]
    let p1_idx = virt.p1_index(); // bits [20:12]

    // ── PML4 → PDPT ──────────────────────────────────────────────────────────
    let p4_entry = &mut pml4[p4_idx];
    let pdpt_phys = ensure_table(p4_entry);

    // ── PDPT → PD ────────────────────────────────────────────────────────────
    let pdpt = table_at(pdpt_phys);
    let p3_entry = &mut pdpt[p3_idx];
    let pd_phys = ensure_table(p3_entry);

    // ── PD → PT ──────────────────────────────────────────────────────────────
    let pd = table_at(pd_phys);
    let p2_entry = &mut pd[p2_idx];

    // Guard: do not overwrite an existing huge page with a 4K mapping.
    assert!(
        !p2_entry.flags().contains(PageTableFlags::HUGE_PAGE),
        "VMM: map_4k on a virtual address covered by a huge page: {:#x}",
        virt.as_u64()
    );

    let pt_phys = ensure_table(p2_entry);

    // ── PT → frame ───────────────────────────────────────────────────────────
    let pt = table_at(pt_phys);
    let p1_entry = &mut pt[p1_idx];

    p1_entry.set_addr(phys, flags | PageTableFlags::PRESENT);
}

/// Maps a single 2 MiB huge page.
///
/// Short-circuits the PT level: the PD entry has `HUGE_PAGE` set and points
/// directly to a 2 MiB-aligned physical frame.
///
/// # Safety
/// - `virt` must be 2 MiB-aligned.
/// - `phys` must be 2 MiB-aligned.
pub unsafe fn map_2m(pml4: &mut PageTable, virt: VirtAddr, phys: PhysAddr, flags: PageTableFlags) {
    debug_assert!(
        virt.as_u64().is_multiple_of(pmm::HUGE_FRAME_SIZE),
        "VMM: map_2m virt {:#x} is not 2 MiB-aligned",
        virt.as_u64()
    );
    debug_assert!(
        phys.as_u64().is_multiple_of(pmm::HUGE_FRAME_SIZE),
        "VMM: map_2m phys {:#x} is not 2 MiB-aligned",
        phys.as_u64()
    );

    let p4_idx = virt.p4_index();
    let p3_idx = virt.p3_index();
    let p2_idx = virt.p2_index();

    let p4_entry = &mut pml4[p4_idx];
    let pdpt_phys = ensure_table(p4_entry);

    let pdpt = table_at(pdpt_phys);
    let p3_entry = &mut pdpt[p3_idx];
    let pd_phys = ensure_table(p3_entry);

    let pd = table_at(pd_phys);
    let p2_entry = &mut pd[p2_idx];

    // HUGE_PAGE bit in a PD entry means "this is a 2 MiB leaf", not a pointer
    // to a PT.
    p2_entry.set_addr(
        phys,
        flags | PageTableFlags::PRESENT | PageTableFlags::HUGE_PAGE,
    );
}

/// Ensures the given page table entry points to a valid child table.
///
/// If the entry is not present, allocates a fresh table and writes its address
/// into the entry.  Returns the physical address of the child table.
///
/// # Safety
/// May call `alloc_table()` (which calls `pmm::alloc_frame()`).
unsafe fn ensure_table(entry: &mut PageTableEntry) -> PhysAddr {
    if entry.flags().contains(PageTableFlags::PRESENT) {
        // Entry already points to a table — extract the address.
        entry.addr()
    } else {
        // Allocate and zero a new table.
        let table_phys = alloc_table();
        entry.set_addr(
            table_phys,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
        );
        table_phys
    }
}

// ---------------------------------------------------------------------------
// TLB management
// ---------------------------------------------------------------------------

/// Flushes a single virtual address from the TLB.
///
/// Must be called after modifying any page table entry that was previously
/// present.  Inserting a new (non-present → present) mapping does not require
/// a flush.
#[inline]
pub fn flush_tlb_page(virt: VirtAddr) {
    // Safety: `invlpg` is a privileged instruction with no memory side effects
    // beyond the TLB.
    unsafe {
        core::arch::asm!(
            "invlpg [{addr}]",
            addr = in(reg) virt.as_u64(),
            options(nostack, preserves_flags)
        );
    }
}

/// Flushes the entire TLB by reloading CR3.
///
/// More expensive than `flush_tlb_page` — use only after bulk remapping.
#[inline]
pub fn flush_tlb_all() {
    // Safety: reading and re-writing CR3 invalidates all non-global TLB
    // entries.  Safe in ring 0 between interrupt-safe boundaries.
    unsafe {
        let (frame, flags) = Cr3::read();
        Cr3::write(frame, flags);
    }
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Builds and loads the kernel's own page tables.
///
/// What we map:
///   A) Identity map the first 4 GiB as huge pages (2 MiB each)
///      — keeps the kernel code/data accessible after we load our CR3.
///   B) Direct physical map: RAM_DIRECT_MAP_BASE + p → physical p
///      — maps all installed RAM as huge pages (2 MiB each).
///      — lets the kernel address any physical frame without a separate
///        "virtual allocation" step.
///
/// Call order in `kernel_main`:
///   1–4. BSS / serial / GDT / IDT / PMM
///   5. **`vmm::init(max_phys_addr_bytes)`**
///
/// # Arguments
/// * `max_phys_addr` — total physical RAM in bytes (from PMM stats or BootInfo).
///   Rounded up internally to the nearest 2 MiB boundary.
///
/// # Safety
/// Loads a new CR3.  After this call the old UEFI mapping is gone; do not
/// access any UEFI-provided pointer unless it is within the first 4 GiB or
/// the direct map.
pub unsafe fn init(max_phys_addr: u64, fb_base: Option<u64>, fb_size: u64) {
    // Active NXE (No-Execute Enable) dans le registre EFER
    Efer::update(|flags| *flags |= EferFlags::NO_EXECUTE_ENABLE);

    let pml4 = &mut PML4.0;

    let kernel_flags =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

    // ── A) Identity map first 4 GiB (2 MiB huge pages) ───────────────────────
    //
    // This keeps the kernel code, serial port MMIO, APIC MMIO, and ACPI tables
    // accessible at their physical addresses after we switch to our CR3.
    //
    // 4 GiB / 2 MiB = 2048 huge pages.
    let identity_end: u64 = 4 * 1024 * 1024 * 1024;
    let mut phys: u64 = 0;
    while phys < identity_end {
        map_2m(pml4, VirtAddr::new(phys), PhysAddr::new(phys), kernel_flags);
        phys += pmm::HUGE_FRAME_SIZE;
    }

    crate::kprintln!(
        "[VMM] identity map: 0x0 – {:#x} (4 GiB, 2 MiB pages)",
        identity_end
    );

    // ── B) Direct physical map at RAM_DIRECT_MAP_BASE ─────────────────────────
    //
    // Maps every 2 MiB chunk of installed RAM at:
    //   virtual = RAM_DIRECT_MAP_BASE + physical
    //
    // This allows the slab allocator and PMM to address any physical frame
    // directly without a separate virtual address allocation.
    //
    // We align `max_phys_addr` up to 2 MiB to avoid a partial huge page.
    let ram_end = align_up_2m(max_phys_addr);
    let mut phys: u64 = 0;
    while phys < ram_end {
        let virt = VirtAddr::new(RAM_DIRECT_MAP_BASE + phys);
        map_2m(pml4, virt, PhysAddr::new(phys), kernel_flags);
        phys += pmm::HUGE_FRAME_SIZE;
    }

    crate::kprintln!(
        "[VMM] direct map: {:#x} – {:#x} ({} MiB RAM, 2 MiB pages)",
        RAM_DIRECT_MAP_BASE,
        RAM_DIRECT_MAP_BASE + ram_end,
        ram_end / (1024 * 1024),
    );
    // ── C) Identity map the Framebuffer (if present and above 4 GiB) ──────────
    if let Some(fb) = fb_base {
        let fb_start = fb & !(pmm::HUGE_FRAME_SIZE - 1);
        let fb_end = align_up_2m(fb + fb_size);

        let mut phys = core::cmp::max(fb_start, 4 * 1024 * 1024 * 1024);

        while phys < fb_end {
            let addr = PhysAddr::new(phys);
            map_2m(pml4, VirtAddr::new(phys), addr, kernel_flags);
            phys += pmm::HUGE_FRAME_SIZE;
        }

        crate::kprintln!(
            "[VMM] framebuffer mapped: 0x{:x} - 0x{:x}",
            fb_start,
            fb_end
        );
    }
    // ── Load our PML4 into CR3 ────────────────────────────────────────────────
    let pml4_phys = static_virt_to_phys(pml4 as *const PageTable);
    let pml4_frame = PhysFrame::<Size4KiB>::containing_address(pml4_phys);

    Cr3::write(pml4_frame, Cr3Flags::empty());

    VMM_ACTIVE = true;
    Cr3::write(pml4_frame, Cr3Flags::empty());
    unsafe {
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }

    crate::kprintln!("[VMM] CR3 loaded — PML4 @ {:#x}", pml4_phys.as_u64());
    crate::kprintln!("[VMM] page tables active.");
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Returns the current PML4 table as a mutable reference.
///
/// Used by the slab allocator to map new heap pages.
///
/// # Safety
/// The returned reference aliases the global `PML4`.  Caller must not create
/// overlapping mutable references.
pub unsafe fn pml4() -> &'static mut PageTable {
    &mut PML4.0
}

/// Rounds `n` up to the nearest multiple of 2 MiB.
#[inline]
fn align_up_2m(n: u64) -> u64 {
    let mask = pmm::HUGE_FRAME_SIZE - 1;
    (n + mask) & !mask
}
