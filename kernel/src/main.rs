// ============================================================
// KernOS Kernel
// ============================================================
//
// This is the kernel entry point.
// The bootloader loads this file and jumps to kernel_main().
//
// For now this is a minimal stub that does nothing.
// It will be expanded brick by brick starting from Brick 1.
//
// Language : Rust (no_std, no_main)
// Target   : x86_64-unknown-none
// Author   : Matéo Reymond (AI assisted)
// ============================================================

// No standard library - there is no OS beneath the kernel.
#![no_std]
// No automatic entry point - we define our own below
#![no_main]

// ------------------------------------------------------------
// Kernel Entry Point
// ------------------------------------------------------------
use shared::BootInfo;

/// First function called by the bootloader after it jumps to the kernel.
///
/// At this point :
///   - UEFI boot services are gone
///   - The CPU is in 64-bit long mode
///   - No interrupts, no memory management, no drivers
///
/// This function must never return.
/// Returning here would jump back into undefined memory.
#[no_mangle]
pub extern "C" fn kernel_main(_boot_info: *const BootInfo) -> ! {
    // TODO : Brick 1 - initialize the kernel environment
    panic!()
}

// ------------------------------------------------------------
// Panic Handler
// ------------------------------------------------------------

/// Called automatically if the kernel panics.
///
/// For now we loop forever.
/// Later we will display an error message and halt the CPU properly.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
