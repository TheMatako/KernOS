// kernel/src/shell/builtins.rs
//
// KernOS built-in shell commands.
//
// Each command is represented as a function `fn cmd_xxx(cmd: &Command)`.
// The command dispatch table mapping strings to these functions is located
// in `shell/mod.rs`.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::parser::Command;
use crate::vfs::{self, InodeKind};
use crate::{kprint, kprintln};

// ---------------------------------------------------------------------------
// help
// ---------------------------------------------------------------------------

/// Displays the list of available built-in commands.
pub fn cmd_help(_cmd: &Command) {
    kprintln!("KernOS Shell — available commands:");
    kprintln!("  help                    — print this help message");
    kprintln!("  clear                   — clear the terminal screen");
    kprintln!("  echo [text...]          — print text to standard output");
    kprintln!("  ls [path]               — list directory contents");
    kprintln!("  cat <file>              — print file contents");
    kprintln!("  mkdir <path>            — create an empty directory");
    kprintln!("  rm <path>               — remove a file or empty directory");
    kprintln!("  mv <src> <dst>          — move or rename a file");
    kprintln!("  stat <path>             — display file metadata");
    kprintln!("  write <file> <text>     — append text to a file");
    kprintln!("  ping <ip>               — send an ICMP Echo Request");
    kprintln!("  ps                      — list active scheduler tasks");
    kprintln!("  memstat                 — display physical memory statistics");
    kprintln!("  netstat                 — display network config and ARP cache");
    kprintln!("  uptime                  — display system uptime (ticks)");
    kprintln!("  reboot                  — restart the machine");
    kprintln!("  halt                    — power off the machine");
}

// ---------------------------------------------------------------------------
// clear
// ---------------------------------------------------------------------------

/// Clears the terminal screen using ANSI escape sequences.
pub fn cmd_clear(_cmd: &Command) {
    // ESC[2J = Clear entire screen
    // ESC[H  = Move cursor to home position (0,0)
    kprint!("\x1b[2J\x1b[H");
}

// ---------------------------------------------------------------------------
// echo
// ---------------------------------------------------------------------------

/// Prints arguments to the serial output, separated by spaces.
pub fn cmd_echo(cmd: &Command) {
    let mut buf = [0u8; super::parser::TOKEN_MAX];
    let n = cmd.args_joined(1, &mut buf);
    if n > 0 {
        let s = core::str::from_utf8(&buf[..n]).unwrap_or("");
        kprintln!("{}", s);
    } else {
        kprintln!();
    }
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

/// Lists the contents of a directory.
pub fn cmd_ls(cmd: &Command) {
    let path = if cmd.argc > 1 { cmd.arg(1) } else { "/" };

    // Verify the path exists and resolve its type.
    match vfs::stat(path) {
        None => {
            kprintln!("ls: cannot access '{}': No such file or directory", path);
            return;
        }
        Some(meta) if meta.kind != InodeKind::Directory => {
            // It's a file, not a directory. Print its metadata directly.
            print_stat_line(path, &meta);
            return;
        }
        _ => {}
    }

    let mut count = 0usize;
    let result = vfs::readdir(path, &mut |entry| {
        let name = entry.name_str();
        // Hide relative traversal links by default
        if name == "." || name == ".." {
            return true;
        }

        let kind_char = match entry.kind {
            InodeKind::Directory => 'd',
            InodeKind::Symlink => 'l',
            InodeKind::Device => 'c',
            InodeKind::File => '-',
        };

        // Query the VFS for the actual file size.
        let mut full_path_buf = [0u8; 512];
        let full_path = join_path(path, name, &mut full_path_buf);
        let size = vfs::stat(full_path).map(|m| m.size).unwrap_or(0);

        kprintln!("  [{}] {:>8} B  {}", kind_char, size, name);
        count += 1;
        true
    });

    match result {
        Ok(_) => kprintln!("  {} entry(ies)", count),
        Err(e) => kprintln!("ls: error reading directory: {}", e),
    }
}

// ---------------------------------------------------------------------------
// cat
// ---------------------------------------------------------------------------

/// Prints the contents of a file to the serial output.
pub fn cmd_cat(cmd: &Command) {
    if cmd.argc < 2 {
        kprintln!("usage: cat <file>");
        return;
    }
    let path = cmd.arg(1);

    match vfs::stat(path) {
        None => {
            kprintln!("cat: '{}': No such file", path);
            return;
        }
        Some(m) if m.kind == InodeKind::Directory => {
            kprintln!("cat: '{}': Is a directory", path);
            return;
        }
        _ => {}
    }

    let mut buf = [0u8; 512];
    let mut off = 0u64;
    let mut total = 0usize;

    loop {
        match vfs::read(path, off, &mut buf) {
            Ok(0) => break, // EOF reached
            Ok(n) => {
                // Write byte by byte to the serial port to handle non-ASCII
                // or specific newline formatting smoothly.
                for &b in &buf[..n] {
                    if b == b'\n' {
                        kprint!("\r\n");
                    } else {
                        kprint!("{}", b as char);
                    }
                }
                off += n as u64;
                total += n;
            }
            Err(e) => {
                kprintln!("\ncat: read error: {}", e);
                return;
            }
        }
    }
    if total == 0 {
        kprintln!("(empty file)");
    } else {
        kprintln!();
    } // Final newline for neatness
}

// ---------------------------------------------------------------------------
// mkdir
// ---------------------------------------------------------------------------

/// Creates a new empty directory.
pub fn cmd_mkdir(cmd: &Command) {
    if cmd.argc < 2 {
        kprintln!("usage: mkdir <path>");
        return;
    }
    match vfs::create(cmd.arg(1), InodeKind::Directory, 0o755) {
        Ok(_) => kprintln!("mkdir: created directory '{}'", cmd.arg(1)),
        Err(e) => kprintln!("mkdir: failed to create '{}': {}", cmd.arg(1), e),
    }
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// Removes a file or an empty directory.
pub fn cmd_rm(cmd: &Command) {
    if cmd.argc < 2 {
        kprintln!("usage: rm <path>");
        return;
    }
    match vfs::remove(cmd.arg(1)) {
        Ok(_) => kprintln!("rm: removed '{}'", cmd.arg(1)),
        Err(e) => kprintln!("rm: cannot remove '{}': {}", cmd.arg(1), e),
    }
}

// ---------------------------------------------------------------------------
// mv
// ---------------------------------------------------------------------------

/// Renames or moves a file.
pub fn cmd_mv(cmd: &Command) {
    if cmd.argc < 3 {
        kprintln!("usage: mv <src> <dst>");
        return;
    }
    match vfs::rename(cmd.arg(1), cmd.arg(2)) {
        Ok(_) => kprintln!("mv: '{}' -> '{}'", cmd.arg(1), cmd.arg(2)),
        Err(e) => kprintln!("mv: cannot move: {}", e),
    }
}

// ---------------------------------------------------------------------------
// stat
// ---------------------------------------------------------------------------

/// Displays low-level VFS metadata for a file or directory.
pub fn cmd_stat(cmd: &Command) {
    if cmd.argc < 2 {
        kprintln!("usage: stat <path>");
        return;
    }
    let path = cmd.arg(1);
    match vfs::stat(path) {
        None => kprintln!("stat: cannot stat '{}': No such file", path),
        Some(m) => print_stat_line(path, &m),
    }
}

fn print_stat_line(path: &str, m: &vfs::InodeMeta) {
    let kind = match m.kind {
        InodeKind::File => "regular file",
        InodeKind::Directory => "directory",
        InodeKind::Symlink => "symbolic link",
        InodeKind::Device => "device node",
    };
    kprintln!("  File: {}", path);
    kprintln!("  Type: {}", kind);
    kprintln!("  Inode: {}", m.inode_nr);
    kprintln!("  Size: {} bytes", m.size);
    kprintln!("  Blocks: {}", m.blocks);
    kprintln!("  Access: {:#o}", m.mode);
    kprintln!("  Uid/Gid: {}/{}", m.uid, m.gid);
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

/// Appends text to a file. Creates the file if it does not exist.
pub fn cmd_write(cmd: &Command) {
    if cmd.argc < 3 {
        kprintln!("usage: write <file> <text>");
        return;
    }
    let path = cmd.arg(1);
    let mut buf = [0u8; super::parser::TOKEN_MAX];
    let n = cmd.args_joined(2, &mut buf);

    // Create the file if it doesn't exist.
    if vfs::stat(path).is_none() {
        if let Err(e) = vfs::create(path, InodeKind::File, 0o644) {
            kprintln!("write: failed to create '{}': {}", path, e);
            return;
        }
    }

    // Append the text followed by a newline.
    let data = &buf[..n];
    let offset = vfs::stat(path).map(|m| m.size).unwrap_or(0);
    if let Err(e) = vfs::write(path, offset, data) {
        kprintln!("write: write error: {}", e);
        return;
    }
    // Append newline safely.
    vfs::write(path, offset + n as u64, b"\n").ok();
    kprintln!("write: {} bytes written to '{}'", n + 1, path);
}

// ---------------------------------------------------------------------------
// ping
// ---------------------------------------------------------------------------

/// Sends 4 ICMP Echo Requests and awaits replies.
pub fn cmd_ping(cmd: &Command) {
    if cmd.argc < 2 {
        kprintln!("usage: ping <ip_address>");
        return;
    }

    // Parse the IPv4 string (format "a.b.c.d").
    let ip_str = cmd.arg(1);
    let ip = match parse_ipv4(ip_str) {
        Some(ip) => ip,
        None => {
            kprintln!("ping: invalid IP address format: '{}'", ip_str);
            return;
        }
    };

    kprintln!("PING {}.{}.{}.{} :", ip[0], ip[1], ip[2], ip[3]);

    // Build the ICMP Echo Request payload (8 bytes header + 32 bytes data).
    // type=8, code=0, checksum=0, id=0x1234, seq=N, data=0xAB×32
    for seq in 1u16..=4 {
        let mut pkt = [0u8; 40];
        pkt[0] = 8; // Type = Echo Request
        pkt[1] = 0; // Code = 0

        // Identifier = 0x1234
        pkt[4] = 0x12;
        pkt[5] = 0x34;

        // Sequence number (big-endian)
        pkt[6..8].copy_from_slice(&seq.to_be_bytes());

        // Padding data
        pkt[8..40].fill(0xAB);

        // Compute ICMP Checksum
        let csum = crate::net::ip::checksum(&pkt);
        pkt[2..4].copy_from_slice(&csum.to_be_bytes());

        unsafe {
            // Transmit via the IP layer.
            if let Err(e) = crate::net::ip::send(ip, crate::net::ip::PROTO_ICMP, &pkt) {
                kprintln!("ping: transmission failed: {}", e);
                continue;
            }

            // Await response (spin-poll for ~500 iterations).
            let replied = false;
            for _ in 0..500 {
                crate::net::poll();
                // We test if a reply arrived by polling the NIC ring buffers.
                // A complete implementation would block the task on an ICMP queue.
                core::hint::spin_loop();
            }

            // Note: The actual incoming ICMP reply will be logged to the console
            // by the ICMP module's receive handler.
            kprintln!("  seq={} : transmitted (check system logs for reply)", seq);
            let _ = replied;
        }
    }
}

// ---------------------------------------------------------------------------
// ps
// ---------------------------------------------------------------------------

/// Lists all active processes in the task scheduler.
pub fn cmd_ps(_cmd: &Command) {
    kprintln!("  TID  STATE      NAME");
    kprintln!("  ---  ---------  ---------------");
    unsafe {
        let sched = &crate::scheduler::SCHEDULER;
        for i in 0..sched.len {
            let task_ptr = sched.tasks[i];
            if task_ptr.is_null() {
                continue;
            }
            let task = &*task_ptr;
            let state = match task.state {
                crate::scheduler::TaskState::Ready => "Ready    ",
                crate::scheduler::TaskState::Running => "Running  ",
                crate::scheduler::TaskState::Blocked => "Blocked  ",
                crate::scheduler::TaskState::Dead => "Dead     ",
            };
            let marker = if i == sched.current { "▶" } else { " " };
            kprintln!("  {}  {}  {} {}", task.id, state, marker, task.name);
        }
        kprintln!("  Total Scheduler Ticks : {}", sched.ticks());
    }
}

// ---------------------------------------------------------------------------
// memstat
// ---------------------------------------------------------------------------

/// Displays physical memory statistics (PMM).
pub fn cmd_memstat(_cmd: &Command) {
    crate::pmm::print_stats();
}

// ---------------------------------------------------------------------------
// netstat
// ---------------------------------------------------------------------------

/// Displays network configuration and ARP resolution cache.
pub fn cmd_netstat(_cmd: &Command) {
    let ip = crate::net::local_ip();
    let gw = crate::net::gateway_ip();
    let mac = crate::net::local_mac();

    kprintln!("  Interface : e1000");
    kprintln!(
        "  MAC       : {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    kprintln!("  IP        : {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
    kprintln!("  Gateway   : {}.{}.{}.{}", gw[0], gw[1], gw[2], gw[3]);
    kprintln!("  NIC Ready : {}", crate::drivers::e1000::is_ready());

    // Display ARP Cache
    kprintln!("  ARP Cache:");
    for entry in crate::net::arp::arp_cache_entries() {
        if entry.valid {
            kprintln!(
                "    {}.{}.{}.{}  ->  {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                entry.ip[0],
                entry.ip[1],
                entry.ip[2],
                entry.ip[3],
                entry.mac[0],
                entry.mac[1],
                entry.mac[2],
                entry.mac[3],
                entry.mac[4],
                entry.mac[5],
            );
        }
    }
}

// ---------------------------------------------------------------------------
// uptime
// ---------------------------------------------------------------------------

/// Displays the time since the system booted, calculated via APIC ticks.
pub fn cmd_uptime(_cmd: &Command) {
    let ticks = unsafe { crate::scheduler::SCHEDULER.ticks() };
    // Assuming a 100 Hz timer tick rate
    let secs = ticks / 100;
    let mins = secs / 60;
    let hours = mins / 60;
    kprintln!(
        "  Uptime: {}h {:02}m {:02}s  ({} ticks at ~100 Hz)",
        hours,
        mins % 60,
        secs % 60,
        ticks
    );
}

// ---------------------------------------------------------------------------
// Power Management (reboot / halt)
// ---------------------------------------------------------------------------

/// Reboots the machine via the legacy 8042 PS/2 keyboard controller.
pub fn cmd_reboot(_cmd: &Command) {
    kprintln!("Rebooting system...");
    unsafe {
        // Method 1: Pulse the CPU reset line via the PS/2 controller (Port 0x64).
        let mut port: x86_64::instructions::port::Port<u8> =
            x86_64::instructions::port::Port::new(0x64);

        // Wait for the controller's input buffer to be empty.
        loop {
            if port.read() & 0x02 == 0 {
                break;
            }
        }
        port.write(0xFE_u8); // 0xFE = System Reset command

        // Method 2: If the PS/2 reset fails, trigger an intentional triple fault.
        core::arch::asm!("ud2", options(noreturn, nostack));
    }
}

/// Halts the machine using QEMU's specific ACPI power-off port, or spins indefinitely.
pub fn cmd_halt(_cmd: &Command) {
    kprintln!("System halting...");
    unsafe {
        // Specific ACPI power-off port for QEMU/Bochs environments.
        let mut port: x86_64::instructions::port::Port<u16> =
            x86_64::instructions::port::Port::new(0x604);
        port.write(0x2000_u16);

        // If ACPI shutdown fails, disable interrupts and halt the CPU in a tight loop.
        x86_64::instructions::interrupts::disable();
        loop {
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }
}

// ---------------------------------------------------------------------------
// Internal Utilities
// ---------------------------------------------------------------------------

/// Parses a string formatted as "a.b.c.d" into a 4-byte array.
/// Returns None if the format is invalid.
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut part = 0u32;
    let mut dots = 0usize;
    let mut has_digits = false;

    for b in s.bytes() {
        match b {
            b'0'..=b'9' => {
                part = part * 10 + (b - b'0') as u32;
                if part > 255 {
                    return None;
                }
                has_digits = true;
            }
            b'.' => {
                if !has_digits || dots >= 3 {
                    return None;
                }
                ip[dots] = part as u8;
                part = 0;
                dots += 1;
                has_digits = false;
            }
            _ => return None, // Invalid character found
        }
    }
    if dots != 3 || !has_digits {
        return None;
    }
    ip[3] = part as u8;
    Some(ip)
}

/// Safely concatenates `base` + "/" + `name` into a pre-allocated stack buffer `out`.
/// Returns the resulting string slice.
fn join_path<'a>(base: &str, name: &str, out: &'a mut [u8; 512]) -> &'a str {
    let mut pos = 0usize;
    let base = base.trim_end_matches('/');

    // Copy base path
    let blen = base.len().min(510);
    out[..blen].copy_from_slice(&base.as_bytes()[..blen]);
    pos += blen;

    // Append separator
    if pos < 511 {
        out[pos] = b'/';
        pos += 1;
    }

    // Copy filename
    let nlen = name.len().min(511 - pos);
    out[pos..pos + nlen].copy_from_slice(&name.as_bytes()[..nlen]);
    pos += nlen;

    core::str::from_utf8(&out[..pos]).unwrap_or("/")
}
