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
//   8. APIC
//   9. SCHEDULER
//  10. SYSCALL
//  11. Enable interrupts  (STI — safe now that IDT is loaded)
//  12. Spin (hlt loop)
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
mod apic;
mod gdt;
mod idt;
mod pmm;
mod scheduler;
mod serial;
mod slab;
mod syscall;
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

/// Demo user-mode task entry point.
///
/// This function will be jumped to from ring 3 via `iretq`.
/// It uses the `syscall` instruction to communicate with the kernel.
///
/// Calling convention from ring 3 (Linux ABI):
///   RAX = syscall number
///   RDI, RSI = arguments
unsafe extern "C" fn user_task_entry() -> ! {
    // sys_write(buf, len) — write "Hello from ring 3!\n" to serial.
    let msg = b"Hello from ring 3!\n";
    core::arch::asm!(
        "syscall",
        in("rax") syscall::SYS_WRITE,
        in("rdi") msg.as_ptr() as u64,
        in("rsi") msg.len() as u64,
        // RCX and R11 are clobbered by the CPU on syscall/sysret.
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );

    // sys_exit(0) — terminate.
    core::arch::asm!(
        "syscall",
        in("rax") syscall::SYS_EXIT,
        in("rdi") 0u64,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );

    // Unreachable — sys_exit never returns.
    loop {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
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

    // ── 8. APIC timer ─────────────────────────────────────────────────────────
    // Must be called BEFORE scheduler::init() so the timer is ticking when
    // we enable interrupts.  The IDT handler (idt.rs) will call scheduler::tick()
    // on every timer interrupt.
    unsafe { apic::init() };

    // ── 9. Scheduler ──────────────────────────────────────────────────────────
    unsafe { scheduler::init() };

    // ── 10. Syscall ───────────────────────────────────────────────────────────
    // Configures STAR / LSTAR / FMASK / EFER.SCE so that ring-3 tasks can
    // use the `syscall` instruction to enter the kernel.
    unsafe { syscall::init() };

    // ── 12. Enable interrupts ─────────────────────────────────────────────────
    x86_64::instructions::interrupts::enable();

    kprintln!("Brick 6 complete — syscall/sysret operational.");
    kprintln!("Ring-3 entry demo: launching user_task_entry via iretq...");

    // ── Demo: jump to ring-3 ──────────────────────────────────────────────────
    //
    // Allocate a small user stack (one frame from the PMM, identity-mapped
    // in the first 4 GiB), then use iretq to drop to ring 3.
    //
    // In a complete OS this would be done inside the scheduler when dispatching
    // a user-mode process for the first time (Brick 7).
    unsafe {
        let user_stack_phys = pmm::alloc_frame().expect("main: OOM for user stack");

        // Stack top = base + 4 KiB (stacks grow downward).
        let user_stack_top = user_stack_phys + pmm::FRAME_SIZE;

        // `user_task_entry` is currently a kernel function address.
        // In a real OS it would live in a separate user address space;
        // here it is identity-mapped (first 4 GiB) so the address is the same
        // in both ring 0 and ring 3 — valid for this demo only.
        syscall::jump_to_usermode(user_task_entry as *const () as u64, user_stack_top);
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
