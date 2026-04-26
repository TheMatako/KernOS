// kernel/src/idt.rs
//
// Interrupt Descriptor Table (IDT).
//
// ── Background ────────────────────────────────────────────────────────────────
//
// The IDT tells the CPU which function to call when an interrupt or exception
// fires.  It has up to 256 entries:
//
//   0–31  : CPU exceptions (defined by Intel — we handle all 32).
//   32–47 : Hardware IRQs remapped from the PIC (or APIC).
//   48–255: Software interrupts / unused.
//
// Each entry is a 16-byte "gate descriptor" that stores:
//   - The address of the handler function.
//   - The code segment selector to switch to (always our kernel CS).
//   - The privilege level required to trigger the gate.
//   - An optional IST index (we use IST 0 for #DF only).
//
// ── Exception vs Interrupt gates ─────────────────────────────────────────────
//
// Interrupt gates automatically clear the IF (Interrupt Flag) on entry,
// preventing nested interrupts.  Exception gates do not.  The x86_64 crate
// uses interrupt gates for all entries, which is the safe default.
//
// ── Error codes ──────────────────────────────────────────────────────────────
//
// Some exceptions push an additional "error code" onto the stack before
// calling the handler.  The x86_64 crate encodes this in the handler
// signature: handlers that receive an error code have a second argument of
// type `u64`.  We must use the exact right signature or the stack frame will
// be misread.
//
// Exceptions WITH error code : #DF(0), #TS, #NP, #SS, #GP, #PF, #AC, #CP.
// Exceptions WITHOUT error code : all others.
//
// ── APIC timer ───────────────────────────────────────────────────────────────
//
// We reserve IRQ vector 0x20 for the Local APIC timer.  The full APIC
// initialisation is deferred to a later brick; here we install a stub handler
// that just sends EOI (End Of Interrupt) so the CPU is not stuck.

#![allow(static_mut_refs)]

use x86_64::{
    instructions::port::Port,
    structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode},
};

use crate::gdt::DOUBLE_FAULT_IST_INDEX;

// ---------------------------------------------------------------------------
// Interrupt vector assignments
// ---------------------------------------------------------------------------

/// Vector number for the Local APIC timer interrupt.
///
/// Vectors 0–31 are reserved for CPU exceptions; we start hardware IRQs at 32
/// (0x20) to avoid collisions.
pub const APIC_TIMER_VECTOR: u8 = 0x20;

/// Vector number for the PS/2 keyboard (IRQ 1 remapped above 0x20).
///
/// We remap IRQ 1 to vector 0x21 to avoid collision with CPU exceptions
/// (0x00–0x1F).  The legacy 8259 PIC is configured accordingly in `init()`.
pub const KEYBOARD_VECTOR: u8 = 0x21;

// ---------------------------------------------------------------------------
// Legacy 8259 PIC — minimal remapping
// ---------------------------------------------------------------------------
//
// Even though we use the APIC for the timer, QEMU still routes the PS/2
// keyboard through the legacy 8259 PIC (IRQ 1).  We must:
//   1. Remap the master PIC so its IRQs start at 0x20 (not 0x08, which
//      collides with CPU exceptions).
//   2. Mask all PIC IRQs except IRQ 1 (keyboard).
//   3. Send PIC EOI at the end of the keyboard handler.
//
// Master PIC ports.
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
// Slave PIC ports.
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// EOI command sent to the 8259 PIC to acknowledge a hardware IRQ.
const PIC_EOI: u8 = 0x20;

/// Remaps the 8259 PIC and masks all IRQs except IRQ 1 (keyboard).
///
/// # Safety
/// Writes to I/O ports. Must be called once during IDT init.
unsafe fn init_pic() {
    let mut p1c: Port<u8> = Port::new(PIC1_CMD);
    let mut p1d: Port<u8> = Port::new(PIC1_DATA);
    let mut p2c: Port<u8> = Port::new(PIC2_CMD);
    let mut p2d: Port<u8> = Port::new(PIC2_DATA);

    // ICW1: start initialisation sequence, expect ICW4.
    p1c.write(0x11);
    p2c.write(0x11);

    // ICW2: remap master to 0x20, slave to 0x28.
    p1d.write(0x20); // master IRQs 0–7  → vectors 0x20–0x27
    p2d.write(0x28); // slave  IRQs 8–15 → vectors 0x28–0x2F

    // ICW3: tell master that slave is on IRQ 2; tell slave its cascade ID.
    p1d.write(0x04); // master: slave on IRQ 2 (bit 2)
    p2d.write(0x02); // slave: cascade identity = 2

    // ICW4: 8086 mode.
    p1d.write(0x01);
    p2d.write(0x01);

    // OCW1: mask all IRQs on both PICs except IRQ 1 (keyboard) on master.
    // Master mask: 0b1111_1101 → only IRQ 1 unmasked.
    // Slave  mask: 0b1111_1111 → all masked (no slave IRQs needed yet).
    p1d.write(0b1111_1101);
    p2d.write(0b1111_1111);
}

// ---------------------------------------------------------------------------
// Static IDT
// ---------------------------------------------------------------------------

/// The global Interrupt Descriptor Table.
///
/// Must be `static` — the CPU keeps the address in the IDTR register and
/// reads from it on every exception.  Moving it would cause a triple fault.
static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialises and loads the IDT.
///
/// Call order in `kernel_main`:
///   1. `zero_bss()`
///   2. `serial::init()`
///   3. `gdt::init()`   ← must come before idt::init
///   4. **`idt::init()`**
///
/// # Safety (internal)
/// Writes to a `static mut` and executes `lidt`.  Must be called exactly once.
pub fn init() {
    unsafe {
        // ── CPU Exceptions (vectors 0–31) ─────────────────────────────────────

        // #DE — Divide Error (vector 0)
        // Triggered by DIV/IDIV with divisor 0 or when the quotient overflows.
        IDT.divide_error.set_handler_fn(handler_divide_error);

        // #DB — Debug (vector 1)
        // Single-step mode, hardware breakpoint, etc.
        IDT.debug.set_handler_fn(handler_debug);

        // NMI — Non-Maskable Interrupt (vector 2)
        // Hardware faults (memory parity, watchdog). Cannot be masked with CLI.
        IDT.non_maskable_interrupt.set_handler_fn(handler_nmi);

        // #BP — Breakpoint (vector 3)
        // Triggered by the INT3 instruction (0xCC), used by debuggers.
        IDT.breakpoint.set_handler_fn(handler_breakpoint);

        // #OF — Overflow (vector 4)
        // Triggered by INTO when the overflow flag is set.
        IDT.overflow.set_handler_fn(handler_overflow);

        // #BR — Bound Range Exceeded (vector 5)
        // BOUND instruction: index outside the declared array bounds.
        IDT.bound_range_exceeded.set_handler_fn(handler_bound_range);

        // #UD — Invalid Opcode (vector 6)
        // Undefined instruction, UD2, or misaligned SSE operand.
        IDT.invalid_opcode.set_handler_fn(handler_invalid_opcode);

        // #NM — Device Not Available (vector 7)
        // FPU/MMX/SSE instruction when CR0.EM or CR0.TS is set.
        IDT.device_not_available
            .set_handler_fn(handler_device_not_available);

        // #DF — Double Fault (vector 8)
        // Triggered when an exception occurs while handling another exception.
        // This handler runs on the IST[0] stack (set in gdt.rs) so that a
        // kernel stack overflow does not immediately cause a triple fault.
        // The error code is always 0 (the CPU pushes it anyway).
        IDT.double_fault
            .set_handler_fn(handler_double_fault)
            // IST index is 1-based in `set_stack_index` (maps to IST[0] in TSS).
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);

        // Vector 9: Coprocessor Segment Overrun — reserved, no longer used.
        // We do not install a handler; the CPU will generate a #GP instead.

        // #TS — Invalid TSS (vector 10)
        IDT.invalid_tss.set_handler_fn(handler_invalid_tss);

        // #NP — Segment Not Present (vector 11)
        IDT.segment_not_present
            .set_handler_fn(handler_segment_not_present);

        // #SS — Stack-Segment Fault (vector 12)
        IDT.stack_segment_fault
            .set_handler_fn(handler_stack_segment_fault);

        // #GP — General Protection Fault (vector 13)
        // The most common kernel exception: null pointer dereference in ring 0,
        // bad segment selector, privileged instruction in user mode, etc.
        IDT.general_protection_fault
            .set_handler_fn(handler_general_protection);

        // #PF — Page Fault (vector 14)
        // Triggered on every unmapped or protection-violating memory access.
        // CR2 holds the faulting virtual address; the error code encodes
        // P/W/U/R/I bits.
        IDT.page_fault.set_handler_fn(handler_page_fault);

        // Vector 15: reserved by Intel. Skip.

        // #MF — x87 Floating-Point Exception (vector 16)
        IDT.x87_floating_point.set_handler_fn(handler_x87_fp);

        // #AC — Alignment Check (vector 17)
        // Misaligned memory access when CR0.AM and EFLAGS.AC are both set.
        IDT.alignment_check.set_handler_fn(handler_alignment_check);

        // #MC — Machine Check (vector 18)
        // Hardware error reported via Model-Specific Registers.
        // This handler must *not* return — the machine state may be corrupt.
        IDT.machine_check.set_handler_fn(handler_machine_check);

        // #XM — SIMD Floating-Point Exception (vector 19)
        IDT.simd_floating_point.set_handler_fn(handler_simd_fp);

        // #VE — Virtualisation Exception (vector 20)
        IDT.virtualization.set_handler_fn(handler_virtualisation);

        // #CP — Control Protection Exception (vector 21) — CET shadow stack.
        IDT.cp_protection_exception
            .set_handler_fn(handler_cp_protection);

        // Vectors 22–27: reserved. Skip.
        // #HV — Hypervisor Injection (vector 28): AMD SVM only.
        // #VC — VMM Communication (vector 29): AMD SEV-ES.
        // #SX — Security Exception (vector 30): AMD only.
        // Vector 31: reserved.

        // ── Hardware IRQ: APIC timer (vector 0x20) ────────────────────────────
        IDT[APIC_TIMER_VECTOR].set_handler_fn(handler_apic_timer);

        // ── Hardware IRQ: PS/2 keyboard (vector 0x21) ─────────────────────────
        IDT[KEYBOARD_VECTOR].set_handler_fn(handler_keyboard);

        // ── Initialise and remap the legacy 8259 PIC ──────────────────────────
        // Must be done before `lidt` so the PIC is configured before any
        // hardware IRQ can fire.
        init_pic();

        // ── Load the IDT (`lidt`) ─────────────────────────────────────────────
        IDT.load();
    }

    crate::kprintln!("[IDT] loaded — 32 exception handlers + APIC timer + keyboard.");
}

// ---------------------------------------------------------------------------
// Exception handlers — no error code
// ---------------------------------------------------------------------------

/// Prints a structured dump of the exception frame.
///
/// All "fatal" handlers call this before looping forever.
fn dump_frame(name: &str, frame: &InterruptStackFrame) {
    crate::kprintln!("--- EXCEPTION: {} ---", name);
    crate::kprintln!("  RIP    = {:#018x}", frame.instruction_pointer.as_u64());
    crate::kprintln!("  CS     = {:#06x}", frame.code_segment.0);
    crate::kprintln!("  RFLAGS = {:#018x}", frame.cpu_flags);
    crate::kprintln!("  RSP    = {:#018x}", frame.stack_pointer.as_u64());
    crate::kprintln!("  SS     = {:#06x}", frame.stack_segment.0);
}

extern "x86-interrupt" fn handler_divide_error(frame: InterruptStackFrame) {
    dump_frame("#DE Divide Error", &frame);
    panic!("#DE at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_debug(frame: InterruptStackFrame) {
    // Debug exceptions are recoverable (single-step). Just log and return.
    crate::kprintln!("[DBG] #DB at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_nmi(frame: InterruptStackFrame) {
    dump_frame("NMI Non-Maskable Interrupt", &frame);
    // NMI indicates hardware failure; halting is the safest response.
    panic!("NMI — possible hardware error");
}

extern "x86-interrupt" fn handler_breakpoint(frame: InterruptStackFrame) {
    // Breakpoints are non-fatal; resume execution after logging.
    crate::kprintln!(
        "[DBG] #BP breakpoint at {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn handler_overflow(frame: InterruptStackFrame) {
    dump_frame("#OF Overflow", &frame);
    panic!("#OF at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_bound_range(frame: InterruptStackFrame) {
    dump_frame("#BR Bound Range Exceeded", &frame);
    panic!("#BR at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_invalid_opcode(frame: InterruptStackFrame) {
    dump_frame("#UD Invalid Opcode", &frame);
    panic!("#UD at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_device_not_available(frame: InterruptStackFrame) {
    dump_frame("#NM Device Not Available", &frame);
    panic!("#NM at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_x87_fp(frame: InterruptStackFrame) {
    dump_frame("#MF x87 Floating-Point", &frame);
    panic!("#MF at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_machine_check(frame: InterruptStackFrame) -> ! {
    // Machine-check exceptions must use the `-> !` signature because
    // returning from a #MC is architecturally undefined.
    dump_frame("#MC Machine Check", &frame);
    panic!("#MC — unrecoverable hardware error");
}

extern "x86-interrupt" fn handler_simd_fp(frame: InterruptStackFrame) {
    dump_frame("#XM SIMD Floating-Point", &frame);
    panic!("#XM at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_virtualisation(frame: InterruptStackFrame) {
    dump_frame("#VE Virtualisation", &frame);
    panic!("#VE at {:#x}", frame.instruction_pointer.as_u64());
}

// ---------------------------------------------------------------------------
// Exception handlers — WITH error code
// ---------------------------------------------------------------------------

/// Prints exception frame + error code, then panics.
fn dump_frame_ec(name: &str, frame: &InterruptStackFrame, error_code: u64) {
    crate::kprintln!("--- EXCEPTION: {} ---", name);
    crate::kprintln!(
        "  RIP        = {:#018x}",
        frame.instruction_pointer.as_u64()
    );
    crate::kprintln!("  CS         = {:#06x}", frame.code_segment.0);
    crate::kprintln!("  RFLAGS     = {:#018x}", frame.cpu_flags);
    crate::kprintln!("  RSP        = {:#018x}", frame.stack_pointer.as_u64());
    crate::kprintln!("  SS         = {:#06x}", frame.stack_segment.0);
    crate::kprintln!("  error_code = {:#018x}", error_code);
}

extern "x86-interrupt" fn handler_double_fault(
    frame: InterruptStackFrame,
    error_code: u64, // always 0 for #DF, but the CPU pushes it anyway
) -> ! {
    // Running on the IST[0] stack — safe even if the kernel stack overflowed.
    dump_frame_ec("#DF Double Fault", &frame, error_code);
    panic!("#DF — unrecoverable");
}

extern "x86-interrupt" fn handler_invalid_tss(frame: InterruptStackFrame, error_code: u64) {
    dump_frame_ec("#TS Invalid TSS", &frame, error_code);
    panic!("#TS selector={:#x}", error_code);
}

extern "x86-interrupt" fn handler_segment_not_present(frame: InterruptStackFrame, error_code: u64) {
    dump_frame_ec("#NP Segment Not Present", &frame, error_code);
    panic!("#NP selector={:#x}", error_code);
}

extern "x86-interrupt" fn handler_stack_segment_fault(frame: InterruptStackFrame, error_code: u64) {
    dump_frame_ec("#SS Stack-Segment Fault", &frame, error_code);
    panic!("#SS at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_general_protection(frame: InterruptStackFrame, error_code: u64) {
    // error_code bits:
    //   [0]    : EXT — external event (e.g. hardware interrupt) caused the fault.
    //   [1]    : IDT — selector points into the IDT rather than the GDT/LDT.
    //   [2]    : TI  — 0 = GDT, 1 = LDT (only when IDT=0).
    //   [15:3] : selector index.
    dump_frame_ec("#GP General Protection Fault", &frame, error_code);
    panic!(
        "#GP at {:#x}  selector_index={}",
        frame.instruction_pointer.as_u64(),
        error_code >> 3
    );
}

extern "x86-interrupt" fn handler_page_fault(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // Read the faulting virtual address from CR2.
    // The x86_64 crate exposes it via `Cr2::read()`.
    use x86_64::registers::control::Cr2;
    let faulting_addr = Cr2::read().expect("CR2 address invalid");

    crate::kprintln!("--- EXCEPTION: #PF Page Fault ---");
    crate::kprintln!("  Faulting address = {:#018x}", faulting_addr.as_u64());
    crate::kprintln!("  Error flags      = {:?}", error_code);
    crate::kprintln!(
        "  RIP              = {:#018x}",
        frame.instruction_pointer.as_u64()
    );
    crate::kprintln!(
        "  RSP              = {:#018x}",
        frame.stack_pointer.as_u64()
    );

    // At this stage we have no page-fault handler; always panic.
    // Brick 4 (VMM) will replace this with demand-paging or CoW logic.
    panic!(
        "#PF at {:#x} accessing {:#x}",
        frame.instruction_pointer.as_u64(),
        faulting_addr.as_u64()
    );
}

extern "x86-interrupt" fn handler_alignment_check(frame: InterruptStackFrame, error_code: u64) {
    dump_frame_ec("#AC Alignment Check", &frame, error_code);
    panic!("#AC at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn handler_cp_protection(frame: InterruptStackFrame, error_code: u64) {
    dump_frame_ec("#CP Control Protection", &frame, error_code);
    panic!("#CP at {:#x}", frame.instruction_pointer.as_u64());
}

// ---------------------------------------------------------------------------
// Hardware IRQ handler — APIC timer
// ---------------------------------------------------------------------------

/// Stub handler for the Local APIC timer (vector 0x20).
///
/// At this stage the APIC is not yet fully configured; this handler simply
/// acknowledges the interrupt and returns so the CPU does not get stuck.
///
/// The full scheduler (Brick 5) will replace this with a proper tick handler.
extern "x86-interrupt" fn handler_apic_timer(_frame: InterruptStackFrame) {
    // ── 1. Acknowledge the interrupt (EOI) ────────────────────────────────────
    //
    // Send End Of Interrupt to the Local APIC *before* calling the scheduler.
    // If we sent EOI after the context switch we might never reach it
    // (the CPU would be running a different task).
    //
    // Safety: apic::eoi() writes to APIC MMIO — valid in ring 0.
    unsafe { crate::apic::eoi() };

    // ── 2. Call the scheduler ─────────────────────────────────────────────────
    //
    // `scheduler::tick()` increments the tick counter and may perform a context
    // switch (switch_context).  If a switch occurs, this handler "returns" into
    // a different task's stack — but that is fine because the interrupt frame
    // was already saved by the CPU before we entered this handler.
    //
    // Safety: we are inside an interrupt handler — IF is already cleared by the
    // CPU, so no nested timer interrupt can fire during the switch.
    unsafe { crate::scheduler::tick() };
}

/// IRQ 1 handler — PS/2 keyboard.
///
/// Reads one scancode byte from port 0x60, decodes it to ASCII, and pushes
/// the result into the keyboard ring buffer for `keyboard::read_char()`.
///
/// We also send EOI to the 8259 master PIC so it can deliver further IRQs.
extern "x86-interrupt" fn handler_keyboard(_frame: InterruptStackFrame) {
    // Decode the scancode and push ASCII into the ring buffer.
    // Safety: reads I/O port 0x60; writes to static mut KB_BUF in keyboard.rs.
    unsafe { crate::drivers::keyboard::handle_irq() };

    // Send EOI to the master 8259 PIC (IRQ 1 is on the master).
    // Without this the PIC will not deliver any further IRQs.
    //
    // Safety: writes to I/O port 0x20.
    unsafe {
        let mut pic1_cmd: Port<u8> = Port::new(PIC1_CMD);
        pic1_cmd.write(PIC_EOI);
    }
}
