// kernel/src/drivers/mod.rs
//
// Kernel driver subsystem — Brick 7.
//
// Sub-modules:
//   keyboard  — PS/2 keyboard (IRQ 1, scancode set 1 → ASCII ring buffer)
//   pci       — PCI bus configuration space enumeration
//   block     — RAM disk block device (512-byte sectors)

#![allow(dead_code)]
#![allow(static_mut_refs)]

pub mod block;
pub mod e1000;
pub mod framebuffer;
pub mod keyboard;
pub mod pci;
/// Initialises all drivers in the correct order.
///
/// Call order in `kernel_main` (after idt::init, before scheduler::init):
///   idt → pmm → vmm → slab → **drivers::init()** → apic → scheduler → …
///
/// # Safety
/// Each sub-driver writes to hardware registers / static mut state.
/// Must be called once, with interrupts disabled.
pub unsafe fn init(ram_disk_size_bytes: usize) {
    keyboard::init();
    pci::init();
    block::init(ram_disk_size_bytes);
    e1000::init();
}
