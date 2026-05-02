// kernel/src/serial.rs
//
// UART 16550 driver for COM1 (I/O base 0x3F8).
//
// After `exit_boot_services()` the UEFI console is gone; the serial port is
// our only debug output channel until a framebuffer text renderer is added.
//
// We use the `x86_64` crate's `Port<u8>` abstraction for all I/O instructions
// instead of raw `asm!("out …")` blocks — this is zero-cost and keeps the
// code readable.
//
// Threading note: the kernel is strictly single-threaded at this stage.
// When SMP is introduced, `SERIAL` must be wrapped in a spinlock.

use core::fmt;
use x86_64::instructions::port::Port;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// I/O base address of COM1 on every x86 PC.
const COM1_BASE: u16 = 0x3F8;

/// Baud-rate divisor for 115 200 baud.
///
/// The 16550 UART has an internal clock of 1 843 200 Hz.
/// Divisor = clock / (16 × baud) = 1 843 200 / (16 × 115 200) = 1.
const BAUD_DIVISOR: u16 = 1;

// ---------------------------------------------------------------------------
// SerialPort struct
// ---------------------------------------------------------------------------

/// A handle to one UART 16550 serial port.
///
/// Each field maps to a specific I/O port offset from the base address.
/// The UART reuses offsets 0 and 1 for two different purposes depending on
/// whether the Divisor Latch Access Bit (DLAB) in the LCR is set:
///
/// | Offset | DLAB=0                  | DLAB=1            |
/// |--------|-------------------------|-------------------|
/// | +0     | Data register (RBR/THR) | Divisor LSB (DLL) |
/// | +1     | Interrupt enable (IER)  | Divisor MSB (DLH) |
pub struct SerialPort {
    /// Offset +0 — data register (TX/RX) **or** divisor latch low byte.
    data: Port<u8>,
    /// Offset +1 — interrupt enable register **or** divisor latch high byte.
    ier: Port<u8>,
    /// Offset +2 — FIFO control register (write) / interrupt ID register (read).
    fcr: Port<u8>,
    /// Offset +3 — line control register (frame format + DLAB flag).
    lcr: Port<u8>,
    /// Offset +4 — modem control register (RTS, DTR, …).
    mcr: Port<u8>,
    /// Offset +5 — line status register (transmit/receive status flags).
    lsr: Port<u8>,
}

impl SerialPort {
    /// Creates a `SerialPort` handle for the given I/O base address.
    ///
    /// This function only constructs the Rust struct; it does **not** touch any
    /// hardware register. Call [`SerialPort::init`] before writing any data.
    ///
    /// # Safety
    /// - `base` must be a valid COM port base address (e.g. `0x3F8` for COM1).
    /// - No other code must concurrently access the same port.
    ///
    /// This constructor is `const` so it can be used to initialise a `static`.
    pub const unsafe fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            ier: Port::new(base + 1),
            fcr: Port::new(base + 2),
            lcr: Port::new(base + 3),
            mcr: Port::new(base + 4),
            lsr: Port::new(base + 5),
        }
    }

    /// Initialises the UART 16550: 115 200 baud, 8 data bits, no parity, 1 stop bit (8N1).
    ///
    /// Must be called exactly **once**, before any call to [`write_byte`].
    ///
    /// # Safety
    /// Writes to hardware I/O ports. No concurrent access to the same COM port is allowed.
    pub unsafe fn init(&mut self) {
        // ── Step 1 ────────────────────────────────────────────────────────────
        // Disable all UART interrupts (IER = 0x00).
        // We will poll the Line Status Register instead of using IRQs, which
        // is simpler and sufficient for a single-threaded debug console.
        self.ier.write(0x00);

        // ── Step 2 ────────────────────────────────────────────────────────────
        // Set DLAB = 1 in the Line Control Register so we can program the
        // baud-rate divisor through offsets +0 (DLL) and +1 (DLH).
        // The rest of the LCR bits don't matter yet; we will overwrite them
        // in step 4.
        self.lcr.write(0x80); // 0b1000_0000 → DLAB = 1

        // ── Step 3 ────────────────────────────────────────────────────────────
        // Write the baud-rate divisor (1 → 115 200 baud) into DLL and DLH.
        // DLL receives the low byte; DLH receives the high byte.
        self.data.write((BAUD_DIVISOR & 0x00FF) as u8); // DLL
        self.ier.write(((BAUD_DIVISOR >> 8) & 0x00FF) as u8); // DLH

        // ── Step 4 ────────────────────────────────────────────────────────────
        // Clear DLAB and set the frame format:
        //   bits [1:0] = 11  → 8 data bits
        //   bit  [2]   =  0  → 1 stop bit
        //   bits [5:3] = 000 → no parity
        //   bit  [7]   =  0  → DLAB cleared (mandatory)
        // Result: LCR = 0x03
        self.lcr.write(0x03);

        // ── Step 5 ────────────────────────────────────────────────────────────
        // Enable and flush the 16-byte transmit/receive FIFOs.
        //   bit [0]   = 1  → enable FIFOs
        //   bit [1]   = 1  → clear receive FIFO
        //   bit [2]   = 1  → clear transmit FIFO
        //   bits[7:6] = 11 → trigger level = 14 bytes
        // Result: FCR = 0xC7
        self.fcr.write(0xC7);

        // ── Step 6 ────────────────────────────────────────────────────────────
        // Enable RTS and DTR in the Modem Control Register so the UART drives
        // the RS-232 handshake lines. Bit 3 enables auxiliary output 2, which
        // is required for interrupt delivery (harmless when IRQs are disabled).
        //   bit [0] = 1 → DTR (Data Terminal Ready)
        //   bit [1] = 1 → RTS (Request To Send)
        //   bit [3] = 1 → OUT2 (enables IRQ line — harmless here)
        // Result: MCR = 0x0B
        self.mcr.write(0x0B);
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    /// Returns `true` when the Transmitter Holding Register is empty (THRE).
    ///
    /// Bit 5 of the Line Status Register is the THRE flag: the UART sets it
    /// when it has finished shifting out the previous byte and is ready to
    /// accept a new one.
    ///
    /// # Safety
    /// Reads an I/O port — same constraints as [`SerialPort::init`].
    unsafe fn transmit_empty(&mut self) -> bool {
        // LSR bit 5 = THRE (Transmitter Holding Register Empty)
        self.lsr.read() & 0x20 != 0
    }

    // -------------------------------------------------------------------------
    // Public write API
    // -------------------------------------------------------------------------

    /// Sends one byte to the serial port.
    ///
    /// Busy-waits (spin-loops) until the UART transmit buffer is empty, then
    /// writes `byte`. For a debug console this is acceptable; a production
    /// driver would use a DMA ring buffer and interrupts.
    ///
    /// # Safety
    /// - Reads and writes hardware I/O ports.
    /// - No concurrent access to the same COM port is allowed.
    pub unsafe fn write_byte(&mut self, byte: u8) {
        // Spin until the UART is ready.
        while !self.transmit_empty() {
            // The `pause` instruction is an x86 hint that we are in a
            // spin-wait loop; it reduces power consumption and avoids
            // memory-order speculation penalties.
            core::hint::spin_loop();
        }
        self.data.write(byte);
    }
}

// ---------------------------------------------------------------------------
// fmt::Write implementation
// ---------------------------------------------------------------------------

/// Implementing `fmt::Write` lets us use the standard `write!` / `writeln!`
/// macros (and `format_args!`) with a `SerialPort`.
///
/// Every `\n` is expanded to `\r\n` so that terminal emulators (e.g. minicom,
/// PuTTY, QEMU's `-serial stdio`) render newlines correctly.
impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                // Safety: single-threaded kernel, COM1 exclusively owned here.
                unsafe { self.write_byte(b'\r') };
            }
            // Safety: same as above.
            unsafe { self.write_byte(byte) };
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

/// The one and only serial port instance used by the kernel.
///
/// # Safety (static mut)
/// The kernel is strictly single-threaded at this stage; there is no risk of
/// a data race. When multi-core support is added, replace this with a
/// `spin::Mutex<SerialPort>` or equivalent.
///
/// We initialise this with `SerialPort::new` which is a `const unsafe fn` —
/// no I/O port access happens here; the hardware is only touched when `init`
/// is called from `kernel_main`.
pub static mut SERIAL: SerialPort = unsafe { SerialPort::new(COM1_BASE) };

/// Initialises the global serial port.
///
/// Must be called exactly once, as early as possible in `kernel_main`,
/// **after** BSS has been zeroed.
///
/// # Safety
/// Writes to hardware I/O ports. Must not be called concurrently.
pub unsafe fn init() {
    #[allow(static_mut_refs)]
    SERIAL.init();
}

/// Formats `args` and writes the result to the global serial port.
///
/// Do not call this directly — use the [`kprint!`] and [`kprintln!`] macros.
///
/// # Safety (internal)
/// Accesses the `static mut SERIAL`. Safe in a single-threaded kernel.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write as _;
    // Safety: single-threaded kernel — no concurrent access to SERIAL.
    unsafe {
        // `write_fmt` can only fail if `write_str` returns `Err`, which our
        // implementation never does, so we silently discard the result.
        #[allow(static_mut_refs)]
        let _ = SERIAL.write_fmt(args);
    }
    // Log to Screen (VNC terminal)
    crate::drivers::framebuffer::print(args);
}
