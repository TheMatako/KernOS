// kernel/src/apic.rs
//
// Local APIC (Advanced Programmable Interrupt Controller) — minimal init.
//
// ── Background ────────────────────────────────────────────────────────────────
//
// Every x86_64 CPU core has its own Local APIC.  It is the hardware block
// responsible for:
//   - Receiving external hardware interrupts from the I/O APIC.
//   - Generating inter-processor interrupts (IPI) for SMP.
//   - Running an internal periodic timer → our scheduler tick.
//
// The Local APIC registers are memory-mapped starting at physical address
// 0xFEE0_0000 (the "xAPIC" mode, which is what we use here).
//
// ── Register map (offsets from APIC_BASE) ────────────────────────────────────
//
//   0x0020  LAPIC_ID         — local APIC ID (read-only)
//   0x0030  LAPIC_VERSION    — version register (read-only)
//   0x00B0  LAPIC_EOI        — End Of Interrupt (write 0 to ack)
//   0x00D0  LAPIC_LDR        — Logical Destination Register
//   0x00E0  LAPIC_DFR        — Destination Format Register
//   0x00F0  LAPIC_SVR        — Spurious Interrupt Vector Register
//   0x0320  LAPIC_TIMER      — LVT Timer register
//   0x0380  LAPIC_TIMER_INIT — Timer Initial Count
//   0x0390  LAPIC_TIMER_CUR  — Timer Current Count (read-only)
//   0x03E0  LAPIC_TIMER_DIV  — Timer Divide Configuration
//
// ── Timer operation ───────────────────────────────────────────────────────────
//
// The APIC timer counts down from LAPIC_TIMER_INIT at a rate of
// (bus_clock / divisor).  When it reaches 0 it fires the vector stored in
// LAPIC_TIMER, then reloads LAPIC_TIMER_INIT (periodic mode).
//
// We cannot know the bus clock frequency without calibrating against an
// external reference (PIT or HPET).  For QEMU we use a fixed initial count
// that produces roughly 100 Hz; real hardware will get a calibrated value
// from the calibrate_timer() function.
//
// ── EOI ──────────────────────────────────────────────────────────────────────
//
// After every APIC interrupt the handler must write 0 to LAPIC_EOI.
// Without this the APIC will not deliver further interrupts at the same or
// lower priority.  The IDT timer handler (idt.rs) does this via `apic::eoi()`.

#![allow(dead_code)]

use core::ptr;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Physical base address of the Local APIC MMIO region.
/// This is architecturally fixed for xAPIC mode (can be relocated via IA32_APIC_BASE MSR,
/// but we keep the default).
const APIC_BASE: u64 = 0xFEE0_0000;

// Register offsets (in bytes from APIC_BASE).
const LAPIC_SVR: u64 = 0x00F0; // Spurious Interrupt Vector Register
const LAPIC_EOI: u64 = 0x00B0; // End Of Interrupt
const LAPIC_TIMER: u64 = 0x0320; // LVT Timer register
const LAPIC_TIMER_INIT: u64 = 0x0380; // Timer Initial Count
const LAPIC_TIMER_DIV: u64 = 0x03E0; // Timer Divide Configuration

// SVR bits.
/// Setting bit 8 in the SVR enables the Local APIC.
const SVR_APIC_ENABLE: u32 = 1 << 8;
/// Spurious vector number (sent when a spurious interrupt occurs).
/// Must be in the range 0xF0–0xFF; we use 0xFF.
const SVR_SPURIOUS_VECTOR: u32 = 0xFF;

// LVT Timer bits.
/// Periodic mode (bit 17 = 1): the timer reloads automatically.
const TIMER_PERIODIC: u32 = 1 << 17;

// Timer divisor: divide bus clock by 16.
// LAPIC_TIMER_DIV value 0x3 = divide by 16.
const TIMER_DIVIDE_BY_16: u32 = 0x3;

/// The interrupt vector delivered when the APIC timer fires.
/// Must match `APIC_TIMER_VECTOR` in idt.rs.
pub const APIC_TIMER_VECTOR: u32 = 0x20;

/// Initial count loaded into the APIC timer.
///
/// With a 1 GHz bus clock and divide-by-16:
///   tick rate = 1_000_000_000 / 16 / TIMER_INIT_COUNT
///
/// For QEMU (virtual bus ≈ 1 GHz) and TIMER_INIT_COUNT = 625_000:
///   tick rate ≈ 100 Hz  (10 ms per tick)
///
/// On real hardware call `calibrate_timer()` to replace this with a measured
/// value before calling `init()`.
const TIMER_INIT_COUNT: u32 = 625_000;

// ---------------------------------------------------------------------------
// Low-level MMIO helpers
// ---------------------------------------------------------------------------

/// Reads a 32-bit value from an APIC register.
///
/// # Safety
/// `offset` must be a valid APIC register offset (multiple of 16).
/// APIC MMIO must be identity-mapped (true after vmm::init maps the first 4 GiB).
#[inline]
unsafe fn apic_read(offset: u64) -> u32 {
    ptr::read_volatile((APIC_BASE + offset) as *const u32)
}

/// Writes a 32-bit value to an APIC register.
///
/// `write_volatile` prevents the compiler from reordering or eliding the write.
///
/// # Safety
/// Same as `apic_read`.
#[inline]
unsafe fn apic_write(offset: u64, value: u32) {
    ptr::write_volatile((APIC_BASE + offset) as *mut u32, value);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sends End Of Interrupt to the Local APIC.
///
/// Must be called at the end of every APIC interrupt handler.
/// Writing any value (we use 0) to the EOI register signals acknowledgement.
///
/// # Safety
/// Writes to APIC MMIO. Call only from an interrupt handler.
#[inline]
pub unsafe fn eoi() {
    apic_write(LAPIC_EOI, 0);
}

/// Initialises the Local APIC and starts the periodic timer.
///
/// Call order in `kernel_main` (after vmm::init — APIC MMIO must be mapped):
///   ...vmm::init() → **apic::init()** → scheduler::init() → sti
///
/// # Safety
/// Writes to APIC MMIO. Must be called exactly once, on the BSP, with
/// interrupts disabled (IF = 0).
pub unsafe fn init() {
    // ── Step 1: Enable the Local APIC via the Spurious Vector Register ────────
    //
    // Bit 8 = APIC Software Enable.
    // Bits 7:0 = spurious interrupt vector (must be ≥ 0x10, conventionally 0xFF).
    // A "spurious" interrupt is delivered when the CPU acknowledges an interrupt
    // that was already retracted by the hardware before the CPU could service it.
    apic_write(LAPIC_SVR, SVR_APIC_ENABLE | SVR_SPURIOUS_VECTOR);

    // ── Step 2: Configure the timer divisor ───────────────────────────────────
    //
    // The APIC timer clock = bus_clock / divisor.
    // Divisor = 16 is a reasonable middle ground: not too fast (avoids
    // excessive interrupt overhead) and not too slow (keeps timer resolution
    // acceptable).
    apic_write(LAPIC_TIMER_DIV, TIMER_DIVIDE_BY_16);

    // ── Step 3: Configure the LVT Timer register ──────────────────────────────
    //
    // bits [7:0]  = interrupt vector delivered when the timer fires.
    // bit  [16]   = 0 → not masked (interrupt is enabled).
    // bit  [17]   = 1 → periodic mode (auto-reload).
    apic_write(LAPIC_TIMER, TIMER_PERIODIC | APIC_TIMER_VECTOR);

    // ── Step 4: Load the initial count → starts the countdown ─────────────────
    //
    // Writing a non-zero value to LAPIC_TIMER_INIT starts the timer immediately.
    apic_write(LAPIC_TIMER_INIT, TIMER_INIT_COUNT);

    crate::kprintln!(
        "[APIC] Local APIC enabled — timer @ vector {:#x}, ~100 Hz (init_count={})",
        APIC_TIMER_VECTOR,
        TIMER_INIT_COUNT,
    );
}

/// Replaces the timer initial count with a calibrated value.
///
/// Call this if you have measured the actual APIC bus frequency (e.g. using
/// the PIT or HPET as a reference).
///
/// `counts_per_tick` = (bus_clock / 16) / desired_hz
///
/// # Safety
/// Writes to APIC MMIO. Must be called with interrupts disabled.
pub unsafe fn set_timer_count(counts_per_tick: u32) {
    apic_write(LAPIC_TIMER_INIT, counts_per_tick);
    crate::kprintln!("[APIC] timer recalibrated: init_count={}", counts_per_tick);
}
