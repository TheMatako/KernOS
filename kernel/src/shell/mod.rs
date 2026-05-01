// kernel/src/shell/mod.rs
//
// KernOS Shell — Brick 10, part 1/2.
//
// ── Design ───────────────────────────────────────────────────────────────────
//
// The shell is a standard scheduled task that operates continuously:
//   1. Prints a prompt ("kernos:/> ").
//   2. Reads a line from the keyboard (blocking via `keyboard::read_char()`).
//   3. Parses the raw input into standard tokens (`shell/parser.rs`).
//   4. Dispatches the parsed command to its implementation (`shell/builtins.rs`).
//   5. Returns to step 1.
//
// Note: There is no fork/exec implemented at this stage. All commands are
// statically linked Rust functions executed in the same kernel context.
// Future versions will implement `execve` to load and run ELF binaries
// as isolated user-mode tasks (Ring 3).
//
// ── Minimalist Readline ──────────────────────────────────────────────────────
//
// `readline()` processes characters individually as they arrive:
// - Backspace (0x08 / 0x7F) : Visually and logically erases the last character.
// - Enter (0x0D / 0x0A)     : Submits the line.
// - Ctrl-C (0x03)           : Cancels the current line input.
// - Ctrl-L (0x0C)           : Clears the screen (ANSI escape).
// - ANSI Escape Sequences (e.g., arrow keys) are consumed and ignored for now.

#![allow(dead_code)]
#![allow(static_mut_refs)]

pub mod builtins;
pub mod parser;

use crate::{kprint, kprintln};
use builtins::*;
use parser::{Command, LINE_MAX};

// ---------------------------------------------------------------------------
// Prompt Configuration
// ---------------------------------------------------------------------------

/// Prints the interactive shell prompt including the current working directory.
///
/// Format: `kernos:/current/path> `
fn print_prompt() {
    // Currently, CWD is hardcoded to the root directory "/".
    // A future `cd` builtin will require maintaining a CWD string in the task state.
    kprint!("\r\nkernos:/> ");
}

// ---------------------------------------------------------------------------
// Readline Implementation
// ---------------------------------------------------------------------------

/// Blocks and reads a line from the keyboard into `buf`.
///
/// Returns the number of valid bytes read (excluding the trailing '\n').
/// Returns 0 if the user inputs an empty line or presses Ctrl-C.
fn readline(buf: &mut [u8; LINE_MAX]) -> usize {
    let mut pos = 0usize;

    loop {
        // Block until a character is available in the keyboard ring buffer.
        let ch = crate::drivers::keyboard::read_char();

        match ch {
            // ── Enter / Carriage Return ──────────────────────────────────────
            b'\r' | b'\n' => {
                kprintln!(); // Move cursor to the next line
                return pos;
            }

            // ── Backspace (Delete) ───────────────────────────────────────────
            0x08 | 0x7F if pos > 0 => {
                pos -= 1;
                kprint!("\x08 \x08");
            }

            // ── Ctrl-C : Cancel Input ────────────────────────────────────────
            0x03 => {
                kprintln!("^C");
                return 0;
            }

            // ── Ctrl-L : Clear Screen ────────────────────────────────────────
            0x0C => {
                // ANSI escape: [2J = clear screen, [H = cursor to top-left
                kprint!("\x1b[2J\x1b[H");
                // Reprint the prompt and the current buffer contents
                print_prompt();
                kprint!("{}", core::str::from_utf8(&buf[..pos]).unwrap_or(""));
            }

            // ── ANSI Escape Sequences (e.g., Arrow Keys) ─────────────────────
            0x1B => {
                // An escape sequence like 'Arrow Up' generates 3 bytes: ESC, [, A.
                // We consume the next two bytes silently to avoid printing garbage.
                let _ = crate::drivers::keyboard::read_char(); // '['
                let _ = crate::drivers::keyboard::read_char(); // direction letter

                // Future Implementation: Command history navigation (Up/Down).
            }

            // ── Standard Printable Characters ────────────────────────────────
            c if (0x20..0x7F).contains(&c) && pos < LINE_MAX - 1 => {
                buf[pos] = c;
                pos += 1;
                kprint!("{}", c as char);
            }

            // All other control characters are silently ignored.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Command Dispatcher
// ---------------------------------------------------------------------------

/// Function signature for a built-in shell command.
type CmdFn = fn(&Command);

/// Static dispatch table mapping command string names to their handler functions.
const COMMANDS: &[(&str, CmdFn)] = &[
    ("help", cmd_help),
    ("clear", cmd_clear),
    ("echo", cmd_echo),
    ("ls", cmd_ls),
    ("cat", cmd_cat),
    ("mkdir", cmd_mkdir),
    ("rm", cmd_rm),
    ("mv", cmd_mv),
    ("stat", cmd_stat),
    ("write", cmd_write),
    ("ping", cmd_ping),
    ("ps", cmd_ps),
    ("memstat", cmd_memstat),
    ("netstat", cmd_netstat),
    ("uptime", cmd_uptime),
    ("reboot", cmd_reboot),
    ("halt", cmd_halt),
];

/// Resolves a parsed `Command` against the dispatch table and executes it.
fn dispatch(cmd: &Command) {
    let name = cmd.name();
    for &(cmd_name, func) in COMMANDS {
        if cmd_name == name {
            func(cmd);
            return;
        }
    }
    // Command not found
    kprintln!("shell: unknown command: '{}'  (type 'help')", name);
}

// ---------------------------------------------------------------------------
// Main Task Loop
// ---------------------------------------------------------------------------

/// Entry point for the Shell task.
///
/// Typically spawned via `scheduler::spawn("shell", shell::run)`.
/// This function never returns.
pub fn run() -> ! {
    // Print the welcome banner.
    kprintln!();
    kprintln!("╔══════════════════════════════════════╗");
    kprintln!("║         KernOS Shell v0.10           ║");
    kprintln!("║  Type 'help' for a list of           ║");
    kprintln!("║  available commands.                 ║");
    kprintln!("╚══════════════════════════════════════╝");
    kprintln!();

    // Attempt to read the Message of the Day (MOTD) from the virtual filesystem.
    let mut motd_buf = [0u8; 256];
    if let Ok(n) = crate::vfs::read("/etc/motd", 0, &mut motd_buf) {
        if n > 0 {
            kprint!("{}", core::str::from_utf8(&motd_buf[..n]).unwrap_or(""));
        }
    }

    let mut line_buf = [0u8; LINE_MAX];

    loop {
        // Pump the network stack.
        // Important: We must drain pending network packets (e.g., incoming pings)
        // before blocking indefinitely on keyboard input.
        unsafe {
            crate::net::poll();
        }

        print_prompt();

        // Zero out the line buffer to prevent artifacting from previous commands.
        line_buf.fill(0);

        let len = readline(&mut line_buf);
        if len == 0 {
            // Empty line (User just pressed Enter or Ctrl-C) → loop back immediately.
            continue;
        }

        // Parse and dispatch.
        let trimmed = &line_buf[..len];
        match parser::parse(trimmed) {
            None => continue,
            Some(cmd) => dispatch(&cmd),
        }
    }
}
