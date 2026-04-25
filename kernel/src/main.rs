// ============================================================
// KernOS Kernel
// ============================================================
//
// This is the kernel entry point.
// The bootloader loads this file and jumps to kernel_main().
//
// Brick 1, 2, 3 and 4
// Execution order in `kernel_main`:
//   1. Zero BSS           (mandatory — must be first)
//   2. Serial (UART)      (our only debug output)
//   3. GDT + TSS          (segment descriptors, IST stack for #DF)
//   4. IDT                (exception / interrupt handlers)
//   5. PMM
//   6. VMM
//   7. SLAB
//   8. Enable interrupts  (STI — safe now that IDT is loaded)
//   9. Spin (hlt loop)
//
// Language : Rust (no_std, no_main)
// Target   : x86_64-unknown-none
// Author   : Matéo Reymond (AI assisted)
// ============================================================

// No standard library - there is no OS beneath the kernel.
#![no_std]
// No automatic entry point - we define our own below
#![no_main]
// Required by the x86_64 crate's `extern "x86-interrupt"` calling convention.
#![feature(abi_x86_interrupt)]

// ------------------------------------------------------------
// Kernel Entry Point
// ------------------------------------------------------------
mod gdt;
mod idt;
mod pmm;
mod serial;
mod slab;
mod vmm;

use shared::BootInfo;

// ---------------------------------------------------------------------------
// Linker symbols (defined in kernel/kernel.ld)
// ---------------------------------------------------------------------------

extern "C" {
    /// First byte of the `.bss` section.
    static __bss_start: u8;
    /// One-past-the-end of the `.bss` section.
    static __bss_end: u8;
}

// ---------------------------------------------------------------------------
// BSS initialisation
// ---------------------------------------------------------------------------

/// Zeros the entire `.bss` section.
///
/// Must be the very first thing called in `kernel_main`.
/// Every `static` / `static mut` that is not explicitly initialised lives in
/// `.bss` and contains garbage until this function runs.
///
/// # Safety
/// - Writes to all memory in `[__bss_start, __bss_end)`.
/// - Caller must ensure the range is valid writable memory (it always is
///   because the linker placed it inside the kernel image).
/// - Must be called exactly once, before any global variable is accessed.
unsafe fn zero_bss() {
    let start = core::ptr::addr_of!(__bss_start) as *mut u8;
    let end = core::ptr::addr_of!(__bss_end) as *mut u8;
    let count = end.offset_from(start) as usize;
    core::ptr::write_bytes(start, 0u8, count);
}

// ---------------------------------------------------------------------------
// Printing macros
// ---------------------------------------------------------------------------

/// Prints a formatted string to COM1 (no trailing newline).
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*))
    };
}

/// Prints a formatted string to COM1 with a trailing newline.
#[macro_export]
macro_rules! kprintln {
    () => ($crate::kprint!("\n"));
    ($($arg:tt)*) => ($crate::kprint!("{}\n", format_args!($($arg)*)));
}

/// First function called by the bootloader after it jumps to the kernel.
///
/// At this point :
///   - UEFI boot services are gone
///   - The CPU is in 64-bit long mode
///   - No interrupts, no memory management, no drivers
///
/// This function must never return.
/// Returning here would jump back into undefined memory.
/// # Safety
#[no_mangle]
pub unsafe extern "sysv64" fn kernel_main(boot_info: *const BootInfo) -> ! {
    // ── 1. Zero BSS ──────────────────────────────────────────────────────────
    unsafe { zero_bss() };

    // ── 2. Initialise serial port ─────────────────────────────────────────────
    unsafe { serial::init() };

    kprintln!("KernOS kernel starting...");
    kprintln!("boot_info @ {:p}", boot_info);

    if boot_info.is_null() {
        panic!("boot_info is null — bootloader bug");
    }

    // ── 3. Load GDT + TSS ────────────────────────────────────────────────────
    gdt::init();

    // ── 4. Load IDT ──────────────────────────────────────────────────────────
    idt::init();

    // ── 5. PMM ────────────────────────────────────────────────────────────────
    //
    // Safety: single-threaded; boot_info is valid (non-null checked above).
    let memory_map = unsafe { &(*boot_info).memory_map };
    unsafe { pmm::init(memory_map) };

    // Print full RAM stats — good sanity check before VMM.
    pmm::print_stats();

    // ── 6. VMM ────────────────────────────────────────────────────────────────
    // We pass total installed RAM (usable frames × 4 KiB) to vmm::init so it
    // knows how large the direct physical map needs to be.
    let installed_ram = pmm::total_usable_frames() as u64 * pmm::FRAME_SIZE;
    unsafe { vmm::init(installed_ram) };

    // ── 7. Slab allocator ─────────────────────────────────────────────────────
    slab::init();

    // ── 8. Enable interrupts ──────────────────────────────────────────────────
    // Safety: IDT is fully loaded; safe to unmask hardware IRQs.
    x86_64::instructions::interrupts::enable();

    kprintln!("Brick 4 complete - VMM operational.");

    // ── 9. Spin forever ───────────────────────────────────────────────────────
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

// ------------------------------------------------------------
// Panic Handler
// ------------------------------------------------------------

/// Called automatically if the kernel panics.
///
/// For now we loop forever.
/// Later we will display an error message and halt the CPU properly.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Disable interrupts so a timer IRQ cannot re-enter the panic path.
    x86_64::instructions::interrupts::disable();

    kprintln!("\n!!! KERNEL PANIC !!!");
    kprintln!("{}", info);

    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
