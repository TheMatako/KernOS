// ============================================================
// KernOS Kernel
// ============================================================
//
// This is the kernel entry point.
// The bootloader loads this file and jumps to kernel_main().
//
// Brick 1 -> 10
// Execution order in `kernel_main`:
//   1. Zero BSS           (mandatory — must be first)
//   2. Serial (UART)      (our primary debug output)
//   3. GDT + TSS          (segment descriptors, IST stack for #DF)
//   4. IDT                (exception / interrupt handlers)
//   5. PMM                (physical memory management)
//   6. VMM                (virtual memory management)
//   7. SLAB               (kernel heap allocator)
//   8. DRIVERS            (keyboard + PCI + block + e1000)
//   9. NET                (TCP/IP stack initialization)
//  10. VFS                (KernFS mounted on "/")
//  11. APIC               (timer initialization)
//  12. SCHEDULER          (task management)
//  13. SYSCALL            (user-mode entry point configuration)
//  14. SPAWN TASKS        (Shell, Network Poller, Heartbeat)
//  15. Enable interrupts  (STI — safe now that IDT is loaded)
//  16. Spin               (hlt loop for the idle task)
//
// Language : Rust (no_std, no_main)
// Target   : x86_64-unknown-none
// Author   : Matéo Reymond (AI assisted)
// ============================================================

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(static_mut_refs)]

// ------------------------------------------------------------
// Kernel Submodules
// ------------------------------------------------------------
mod apic;
mod drivers;
mod gdt;
mod idt;
mod net;
mod pmm;
mod scheduler;
mod serial;
mod shell;
mod slab;
mod syscall;
mod vfs;
mod vmm;

use shared::BootInfo;

// ---------------------------------------------------------------------------
// Linker Symbols (defined in kernel/kernel.ld)
// ---------------------------------------------------------------------------

extern "C" {
    /// First byte of the `.bss` section.
    static __bss_start: u8;
    /// One-past-the-end of the `.bss` section.
    static __bss_end: u8;
}

// ---------------------------------------------------------------------------
// BSS Initialization
// ---------------------------------------------------------------------------

/// Zeros the entire `.bss` section.
///
/// Must be the very first thing called in `kernel_main`.
/// Every `static` / `static mut` that is not explicitly initialized lives in
/// `.bss` and contains garbage until this function runs.
///
/// # Safety
/// - Writes to all memory in `[__bss_start, __bss_end)`.
/// - Caller must ensure the range is valid writable memory.
unsafe fn zero_bss() {
    let start = core::ptr::addr_of!(__bss_start) as *mut u8;
    let end = core::ptr::addr_of!(__bss_end) as *mut u8;
    let count = end.offset_from(start) as usize;
    core::ptr::write_bytes(start, 0u8, count);
}

// ---------------------------------------------------------------------------
// Printing Macros
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

// ---------------------------------------------------------------------------
// Static Instances
// ---------------------------------------------------------------------------

/// Static instance of the root filesystem. Must survive for the lifetime of the VFS.
static mut KERNFS_INSTANCE: core::mem::MaybeUninit<vfs::kernfs::KernFs> =
    core::mem::MaybeUninit::uninit();

// ---------------------------------------------------------------------------
// Background Tasks
// ---------------------------------------------------------------------------

/// The main interactive shell task.
/// Wrapped to match the `fn() -> !` signature expected by the scheduler.
fn task_shell() -> ! {
    shell::run()
}

/// Network polling daemon.
/// Continuously checks the e1000 RX rings for incoming packets.
fn task_net_poll() -> ! {
    let mut ticks: u64 = 0;
    loop {
        unsafe {
            net::poll();
        }
        ticks += 1;

        // Future: Print silent network statistics every 10,000 ticks.
        if ticks.is_multiple_of(1000) {
            // Placeholder for network stats logic
        }

        // Yield the CPU to the next task
        scheduler::schedule();
    }
}

// ---------------------------------------------------------------------------
// Kernel Main
// ---------------------------------------------------------------------------

/// First function called by the bootloader after jumping into the kernel.
///
/// # Safety
/// This function sets up the entire global state of the operating system
/// and must never return.
#[no_mangle]
pub unsafe extern "sysv64" fn kernel_main(boot_info: *const BootInfo) -> ! {
    // ── 1. Zero BSS ──────────────────────────────────────────────────────────
    unsafe { zero_bss() };

    // ── 2. Initialize Serial Port ────────────────────────────────────────────
    unsafe { serial::init() };
    if let Some(fb) = (*boot_info).framebuffer {
        crate::drivers::framebuffer::init(fb);
    }

    kprintln!("KernOS kernel starting...");
    kprintln!("boot_info @ {:p}", boot_info);

    if boot_info.is_null() {
        panic!("boot_info is null — bootloader bug");
    }

    // ── 3. Load GDT + TSS ────────────────────────────────────────────────────
    gdt::init();

    // ── 4. Load IDT ──────────────────────────────────────────────────────────
    idt::init();

    // ── 5. PMM (Physical Memory Management) ──────────────────────────────────
    unsafe { pmm::init(&(*boot_info)) };
    pmm::print_stats();

    // ── 6. VMM (Virtual Memory Management) ───────────────────────────────────
    // ── X. Extraire les infos de l'écran pour le VMM ──────────────────────────
    // ── X. Extraire les infos pour le VMM ──────────────────────────
    let mut max_phys_addr: u64 = 0;

    // On cherche l'adresse physique la plus lointaine déclarée par le BIOS
    for region in (*boot_info).memory_map.valid_entries() {
        let end = region.base + region.length;
        if end > max_phys_addr {
            max_phys_addr = end;
        }
    }

    let fb_base = (*boot_info).framebuffer.map(|fb| fb.base);
    let fb_size = (*boot_info)
        .framebuffer
        .map(|fb| fb.size as u64)
        .unwrap_or(0);

    // Initialiser le VMM avec la RAM et le Framebuffer

    vmm::init(max_phys_addr, fb_base, fb_size);

    let boot_info = unsafe {
        let phys_addr = boot_info as *const _ as u64;
        let virt_addr = phys_addr + vmm::RAM_DIRECT_MAP_BASE;
        &*(virt_addr as *const shared::BootInfo) // Adapte "shared::BootInfo" si besoin
    };

    // ── X. Wipe the screen (Clear UEFI leftovers) ────────────────────────────
    if let Some(fb) = boot_info.framebuffer {
        unsafe {
            // Attention : Si ton VMM ne fait pas de direct-mapping sur cette adresse,
            // il faudra demander au VMM de mapper `fb.base` avant de faire ça !
            let fb_ptr = fb.base as *mut u8;

            // On remplit tout le framebuffer avec des 0 (Noir absolu)
            core::ptr::write_bytes(fb_ptr, 0, fb.size);
        }
    }
    // ── 7. Slab Allocator ────────────────────────────────────────────────────
    slab::init();
    unsafe {
        drivers::framebuffer::enable_double_buffering();
    }
    // ── 8. Drivers & Hardware ────────────────────────────────────────────────
    // Allocate 32 MiB of contiguous physical memory for the RAM disk.
    const RAM_DISK_SIZE: usize = 32 * 1024 * 1024;
    unsafe { drivers::init(RAM_DISK_SIZE) };

    // kprintln!("[PCI]  device list:");
    // for dev in drivers::pci::devices() {
    //     kprintln!(
    //         "  {:02x}:{:02x}.{}  {:04x}:{:04x}  {}",
    //         dev.bus,
    //         dev.slot,
    //         dev.func,
    //         dev.vendor_id,
    //         dev.device_id,
    //         dev.class_name(),
    //     );
    // }

    drivers::e1000::init();

    // ── 9. Network Stack ─────────────────────────────────────────────────────
    unsafe {
        net::init();

        // Broadcast an initial ARP request to resolve the default gateway
        net::arp::request(net::gateway_ip());
        for _ in 0..200 {
            net::poll();
        }
    }

    // ── 10. VFS + KernFS Initialization ──────────────────────────────────────
    unsafe {
        let fs = vfs::kernfs::init();
        KERNFS_INSTANCE.write(fs);
        // Mount the filesystem at the root path "/"
        vfs::mount("/", KERNFS_INSTANCE.as_mut_ptr());
    }

    // Populate the filesystem with default configuration files
    vfs::create("/etc/motd", vfs::InodeKind::File, 0o644).ok();
    vfs::write("/etc/motd", 0, b"Welcome to KernOS v0.10 !\n").ok();
    vfs::create("/etc/hostname", vfs::InodeKind::File, 0o644).ok();
    vfs::write("/etc/hostname", 0, b"kernos\n").ok();
    vfs::create("/etc/resolv.conf", vfs::InodeKind::File, 0o644).ok();
    vfs::write("/etc/resolv.conf", 0, b"nameserver 10.0.2.3\n").ok();
    vfs::create("/home/README", vfs::InodeKind::File, 0o644).ok();
    vfs::write(
        "/home/README",
        0,
        b"This is your home directory.\nType 'help' to get started.\n",
    )
    .ok();

    // ── 11. APIC Timer ───────────────────────────────────────────────────────
    unsafe { apic::init() };

    // ── 12. Task Scheduler ───────────────────────────────────────────────────
    unsafe { scheduler::init() };

    // ── 13. Syscall Interface ────────────────────────────────────────────────
    unsafe { syscall::init() };

    // ── 14. Spawn System Tasks ───────────────────────────────────────────────
    unsafe {
        scheduler::spawn("shell", task_shell);
        scheduler::spawn("net_poll", task_net_poll);
    }

    // ── 15. Enable Interrupts ────────────────────────────────────────────────
    // Interrupts are enabled. The timer will now fire and trigger context switches.
    x86_64::instructions::interrupts::enable();

    kprintln!("Brick 10 complete — shell + musl syscalls operational.");
    kprintln!("The interactive shell will appear momentarily...");

    // ── 16. Idle Loop ────────────────────────────────────────────────────────
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

// ------------------------------------------------------------
// Panic Handler
// ------------------------------------------------------------

/// Called automatically if the kernel panics.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Disable interrupts to prevent timer IRQs from re-entering the panic path.
    x86_64::instructions::interrupts::disable();

    kprintln!("\n!!! KERNEL PANIC !!!");
    kprintln!("{}", info);

    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
