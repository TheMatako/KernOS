// kernel/src/drivers/keyboard.rs
//
// PS/2 keyboard driver.
//
// ── Hardware recap ────────────────────────────────────────────────────────────
//
// The PS/2 controller sits on two I/O ports:
//
//   0x60  DATA port   — read: scancode / write: command to keyboard
//   0x64  STATUS port (read) / COMMAND port (write)
//
// When a key is pressed or released, the keyboard sends one or more bytes
// (scancodes) to the controller, which asserts IRQ 1.  The IDT handler reads
// a byte from 0x60 and pushes it into our ring buffer.
//
// ── Scancode set ─────────────────────────────────────────────────────────────
//
// We use Scancode Set 1 (the default for most PS/2 keyboards and for QEMU).
// A *make* code (key press)   = byte with bit 7 = 0.
// A *break* code (key release) = make code | 0x80.
//
// Extended keys (arrows, numpad, …) send 0xE0 followed by the extended byte.
// We handle the simple US-QWERTY make codes here; a full driver would also
// track modifier state (shift, ctrl, alt).
//
// ── Ring buffer ───────────────────────────────────────────────────────────────
//
// The IRQ handler pushes decoded ASCII bytes into a 256-byte circular buffer.
// `keyboard::read_char()` pops from the front (blocking spin if empty).
// This decouples the IRQ latency from the consumer (the shell).

#![allow(dead_code)]
#![allow(static_mut_refs)]

use x86_64::instructions::port::Port;

// ---------------------------------------------------------------------------
// I/O ports
// ---------------------------------------------------------------------------

/// PS/2 data port — read scancodes, write keyboard commands.
const PS2_DATA: u16 = 0x60;
/// PS/2 status port (read) / command port (write).
const PS2_STATUS: u16 = 0x64;

/// Status register bit: output buffer full (safe to read from 0x60).
const STATUS_OUTPUT_FULL: u8 = 1 << 0;

// ---------------------------------------------------------------------------
// Ring buffer
// ---------------------------------------------------------------------------

/// Capacity of the ASCII ring buffer (must be a power of two).
const BUF_SIZE: usize = 256;

/// Tracks the state of modifier keys.
static mut KB_SHIFT: bool = false;
static mut KB_CTRL: bool = false;
static mut KB_ALTGR: bool = false;

/// Circular byte buffer for decoded ASCII characters.
struct RingBuffer {
    buf: [u8; BUF_SIZE],
    head: usize, // next write index
    tail: usize, // next read  index
}

impl RingBuffer {
    const fn new() -> Self {
        Self {
            buf: [0u8; BUF_SIZE],
            head: 0,
            tail: 0,
        }
    }

    /// Returns `true` if there are no bytes to read.
    fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    /// Returns `true` if the buffer is completely full.
    fn is_full(&self) -> bool {
        (self.head + 1) % BUF_SIZE == self.tail
    }

    /// Pushes one byte.  Silently drops it if the buffer is full
    /// (the shell is too slow — should not happen at human typing speed).
    fn push(&mut self, byte: u8) {
        if !self.is_full() {
            self.buf[self.head] = byte;
            self.head = (self.head + 1) % BUF_SIZE;
        }
    }

    /// Pops one byte, or returns `None` if empty.
    fn pop(&mut self) -> Option<u8> {
        if self.is_empty() {
            None
        } else {
            let byte = self.buf[self.tail];
            self.tail = (self.tail + 1) % BUF_SIZE;
            Some(byte)
        }
    }
}

// ---------------------------------------------------------------------------
// Scancode → ASCII table (US-QWERTY, scancode set 1, unshifted)
// ---------------------------------------------------------------------------

// Index = scancode make byte (0x00–0x58).
// Value = ASCII character, or 0x00 for non-printable / unmapped.
// Layout: AZERTY-FR (Unaccented to fit in single-byte ASCII)
#[rustfmt::skip]
const SCANCODE_TO_ASCII: [u8; 89] = [
//  0     1     2     3     4     5     6     7     8     9     A     B     C     D     E     F
    0,    0,    b'&', b'e', b'"', b'\'',b'(', b'-', b'e', b'_', b'c', b'a', b')', b'=', 0x08, b'\t', // 0x00–0x0F  (0x08=BS)
    b'a', b'z', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'^', b'$', b'\n', 0,   b'q', b's',  // 0x10–0x1F
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b'm', b'u', b'*', 0,    b'\\',b'w', b'x', b'c', b'v',  // 0x20–0x2F
    b'b', b'n', b',', b';', b':', b'!', 0,    b'*', 0,    b' ', 0,    0,    0,    0,    0,    0,    // 0x30–0x3F
    0,    0,    0,    0,    0,    0,    0,    b'7', b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1',  // 0x40–0x4F
    b'2', b'3', b'0', b'.',  0,    0,    0,    0,    0,                                              // 0x50–0x58
];

// ---------------------------------------------------------------------------
// Static state
// ---------------------------------------------------------------------------

/// The decoded-ASCII ring buffer — filled by IRQ 1, drained by `read_char()`.
static mut KB_BUF: RingBuffer = RingBuffer::new();

/// When `true`, the next scancode byte is the second byte of an 0xE0 extended
/// key sequence.  We skip extended keys for now (arrows, F-keys, etc.).
static mut KB_EXTENDED: bool = false;

// ---------------------------------------------------------------------------
// IRQ handler (called from idt.rs)
// ---------------------------------------------------------------------------

/// Reads one scancode byte from the PS/2 data port and decodes it.
///
/// Called from the IRQ 1 handler registered in `idt.rs`.
///
/// # Safety
/// Reads I/O port 0x60. Writes to `static mut KB_BUF` and `KB_EXTENDED`.
/// Must be called only from the keyboard IRQ handler (interrupts masked).
pub unsafe fn handle_irq() {
    let mut data_port: Port<u8> = Port::new(PS2_DATA);
    let scancode: u8 = data_port.read();

    // 1. Handle Extended Sequence (0xE0 prefix)
    if scancode == 0xE0 {
        KB_EXTENDED = true;
        return;
    }

    // 2. Handle Modifiers (Shift, Ctrl, AltGr)
    match scancode {
        // --- SHIFT ---
        0x2A | 0x36 => {
            KB_SHIFT = true;
            return;
        } // Pressed
        0xAA | 0xB6 => {
            KB_SHIFT = false;
            return;
        } // Released

        // --- CTRL ---
        0x1D => {
            KB_CTRL = true;
            return;
        } // Pressed
        0x9D => {
            KB_CTRL = false;
            return;
        } // Released

        // --- ALT GR (E0 38) ---
        0x38 if KB_EXTENDED => {
            KB_ALTGR = true;
            KB_EXTENDED = false; // Reset here since we handled it
            return;
        }
        0xB8 if KB_EXTENDED => {
            KB_ALTGR = false;
            KB_EXTENDED = false; // Reset here since we handled it
            return;
        }

        _ => {}
    }

    // 3. Skip remaining extended bytes (arrows, etc.) if not handled above
    if KB_EXTENDED {
        KB_EXTENDED = false;
        return;
    }

    // 4. Break code (key release) -> ignore
    if scancode & 0x80 != 0 {
        return;
    }

    // 5. Decode Make Code
    let idx = scancode as usize;
    if idx < SCANCODE_TO_ASCII.len() {
        let mut ascii = SCANCODE_TO_ASCII[idx];

        // --- LAYER: SHIFT (Numbers & Punctuation) ---
        if KB_SHIFT {
            ascii = match scancode {
                0x02..=0x0B => b"1234567890"[idx - 0x02], // Range 0x02 to 0x0B
                0x33 => b'.', // Shift + ',' = '.' (Very important for IPs!)
                0x34 => b'/', // Shift + ';' = '/'
                0x35 => b'?', // Shift + ':' = '?'
                0x0C => 176,  // Degree symbol °
                0x0D => b'+',
                _ if ascii.is_ascii_lowercase() => ascii - 32,
                _ => ascii,
            };
        }
        // --- LAYER: ALTGR (Special symbols) ---
        else if KB_ALTGR {
            ascii = match scancode {
                0x03 => b'~',
                0x04 => b'#',
                0x05 => b'{',
                0x06 => b'[',
                0x07 => b'|',
                0x08 => b'`',
                0x09 => b'\\',
                0x0A => b'^',
                0x0B => b'@',
                0x0C => b']',
                0x0D => b'}',
                _ => 0,
            };
        }

        // --- LAYER: CONTROL (ASCII Control Codes) ---
        // If Ctrl is held, translate 'a'-'z' to 1-26
        if KB_CTRL && (ascii.to_ascii_lowercase()).is_ascii_lowercase() {
            ascii = ascii.to_ascii_lowercase() - b'a' + 1;
        }

        if ascii != 0 {
            KB_BUF.push(ascii);
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Reads one ASCII character from the keyboard buffer.
///
/// Spin-waits (busy-polls) until a character is available.
/// This is the blocking interface used by the shell (Brick 10).
///
/// A non-blocking variant (`try_read_char`) is available for polling loops.
///
/// # Safety
/// Reads `static mut KB_BUF`. Safe in a single-threaded kernel.
pub fn read_char() -> u8 {
    loop {
        // Re-enable interrupts briefly so the IRQ handler can fill the buffer,
        // then check.  `without_interrupts` would deadlock here.
        let ch = unsafe { x86_64::instructions::interrupts::without_interrupts(|| KB_BUF.pop()) };
        if let Some(c) = ch {
            return c;
        }
        // Yield the CPU while waiting — avoids burning 100% of the core.
        crate::scheduler::schedule();
    }
}

/// Non-blocking read: returns `Some(char)` if a key is available, else `None`.
///
/// # Safety
/// Reads `static mut KB_BUF`. Safe in a single-threaded kernel.
pub fn try_read_char() -> Option<u8> {
    unsafe { x86_64::instructions::interrupts::without_interrupts(|| KB_BUF.pop()) }
}

/// Initialises the PS/2 keyboard driver.
///
/// Flushes any stale byte in the output buffer so the first IRQ is clean.
///
/// # Safety
/// Reads I/O port 0x60/0x64. Must be called once with interrupts disabled.
pub unsafe fn init() {
    let mut status_port: Port<u8> = Port::new(PS2_STATUS);
    let mut data_port: Port<u8> = Port::new(PS2_DATA);

    // Drain any stale byte already sitting in the controller output buffer.
    // The STATUS_OUTPUT_FULL bit tells us if there is a byte to read.
    if status_port.read() & STATUS_OUTPUT_FULL != 0 {
        let _ = data_port.read();
    }

    crate::kprintln!("[KBD]  PS/2 keyboard driver ready (IRQ 1, scancode set 1, AZERTY-FR)");
}
