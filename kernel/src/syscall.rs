// kernel/src/syscall.rs
//
// System call interface — Brick 6 & 10.
//
// ── Mechanism: syscall / sysret ──────────────────────────────────────────────
//
// On x86_64, user-mode code executes `syscall` to enter the kernel. This is
// faster than the legacy `int 0x80` approach because it bypasses the IDT lookup
// and the privilege-level checks performed by interrupt gates.
//
// The CPU automatically performs the following on `syscall`:
//   1. Saves RIP  → RCX  (Return address for sysret)
//   2. Saves RFLAGS → R11
//   3. Clears RFLAGS bits defined in IA32_FMASK (e.g., clearing IF to disable IRQs)
//   4. Loads CS  from STAR[47:32]     (Kernel code selector)
//   5. Loads SS  from STAR[47:32] + 8 (Kernel data selector)
//   6. Jumps to  LSTAR                (Our assembly syscall entry point)
//
// On `sysretq` (64-bit return):
//   1. Restores RIP    from RCX
//   2. Restores RFLAGS from R11
//   3. Loads CS from STAR[63:48] + 16 (User code selector)
//   4. Loads SS from STAR[63:48] + 8  (User data selector)
//   5. Drops privilege level back to Ring 3
//
// ── MSRs Used ────────────────────────────────────────────────────────────────
//
//   IA32_STAR  (0xC000_0081) — Segment selectors for syscall/sysret
//   IA32_LSTAR (0xC000_0082) — 64-bit syscall entry point (RIP)
//   IA32_FMASK (0xC000_0084) — RFLAGS bits to clear on entry
//   IA32_EFER  (0xC000_0080) — Extended Feature Enable Register (SCE bit)
//
// ── Syscall ABI (Linux x86_64 Compatibility) ─────────────────────────────────
//
// We follow the standard Linux x86_64 syscall ABI to ensure compatibility
// with `musl libc` and standard C programs:
//
//   RAX = Syscall number (on entry) / Return value (on exit)
//   RDI = Argument 1
//   RSI = Argument 2
//   RDX = Argument 3
//   R10 = Argument 4   (Note: NOT RCX, as RCX is clobbered by the CPU)
//   R8  = Argument 5
//   R9  = Argument 6

#![allow(dead_code)]
#![allow(static_mut_refs)]

use crate::gdt::{KCODE_SELECTOR, STAR_USER_BASE};
use x86_64::registers::model_specific::{Efer, EferFlags, Msr};

// ---------------------------------------------------------------------------
// MSR Addresses (Intel SDM Vol. 4)
// ---------------------------------------------------------------------------

/// IA32_EFER — Extended Feature Enable Register.
/// Bit 0 (SCE) must be set to enable `syscall`/`sysret` instructions.
const MSR_EFER: u32 = 0xC000_0080;

/// IA32_STAR — Syscall Target Address Register.
/// bits [63:48] = User segment base (for sysret)
/// bits [47:32] = Kernel CS (for syscall)
/// bits [31:0]  = Legacy 32-bit EIP (ignored in 64-bit mode)
const MSR_STAR: u32 = 0xC000_0081;

/// IA32_LSTAR — Long Mode STAR — 64-bit syscall entry point.
const MSR_LSTAR: u32 = 0xC000_0082;

/// IA32_FMASK — RFLAGS mask applied on syscall entry.
/// Bits set here are cleared in RFLAGS when the syscall fires.
/// We clear IF (bit 9) to prevent timer preemptions during the entry stub.
const MSR_FMASK: u32 = 0xC000_0084;

// ---------------------------------------------------------------------------
// RFLAGS Bits
// ---------------------------------------------------------------------------

/// Interrupt Flag — bit 9 of RFLAGS.
const RFLAGS_IF: u64 = 1 << 9;

// ---------------------------------------------------------------------------
// Syscall Numbers (Linux ABI mapping)
// ---------------------------------------------------------------------------

pub const SYS_WRITE: u64 = 0;
pub const SYS_EXIT: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_SLEEP_TICKS: u64 = 4;

pub const SYS_OPEN: u64 = 5;
pub const SYS_CLOSE: u64 = 6;
pub const SYS_READ_FD: u64 = 7;
pub const SYS_WRITE_FD: u64 = 8;
pub const SYS_LSEEK: u64 = 9;
pub const SYS_FSTAT: u64 = 10;
pub const SYS_BRK: u64 = 11;
pub const SYS_MMAP: u64 = 12;
pub const SYS_MUNMAP: u64 = 13;

/// Magic return value indicating an unimplemented syscall.
/// Mirrors Linux's `-ENOSYS` in two's-complement u64.
const ENOSYS: u64 = u64::MAX - 38 + 1; // -38

// ---------------------------------------------------------------------------
// Saved Register Frame
// ---------------------------------------------------------------------------

/// Represents all registers pushed by the assembly syscall entry stub.
///
/// `#[repr(C)]` guarantees fields match the exact stack layout created by
/// the pushes in `global_asm!`.
#[repr(C)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    /// Saved user RIP (clobbered into RCX by CPU)
    pub rcx: u64,
    pub rdx: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    /// Saved user RFLAGS (clobbered into R11 by CPU)
    pub r11: u64,
    /// Syscall number (RAX on entry; modified to hold the return value)
    pub rax: u64,
}

// ---------------------------------------------------------------------------
// Syscall Entry Stub (Assembly)
// ---------------------------------------------------------------------------

core::arch::global_asm!(
    ".global syscall_entry",
    ".type syscall_entry, @function",
    "syscall_entry:",
    // Save state (build SyscallFrame on the kernel stack)
    "push rax", // syscall number
    "push r11", // saved user RFLAGS
    "push r10", // arg4
    "push r9",  // arg6
    "push r8",  // arg5
    "push rsi", // arg2
    "push rdi", // arg1
    "push rdx", // arg3
    "push rcx", // saved user RIP
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15", // RSP points here now
    // Call Rust dispatcher
    "mov rdi, rsp",          // arg1: &SyscallFrame
    "mov rsi, [rsp + 14*8]", // arg2: syscall number (from frame.rax)
    "call syscall_dispatch",
    // Store return value into frame.rax
    "mov [rsp + 14*8], rax",
    // Restore state
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "pop rcx", // restore user RIP
    "pop rdx",
    "pop rdi",
    "pop rsi",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11", // restore user RFLAGS
    "pop rax", // pop return value
    // Return to ring 3
    "sysretq",
);

extern "C" {
    fn syscall_entry();
}

// ---------------------------------------------------------------------------
// Rust Syscall Dispatcher
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn syscall_dispatch(frame: *mut SyscallFrame, nr: u64) -> u64 {
    match nr {
        SYS_WRITE => sys_write(&*frame),
        SYS_EXIT => sys_exit(&*frame),
        SYS_YIELD => sys_yield(),
        SYS_GETPID => sys_getpid(),
        SYS_SLEEP_TICKS => sys_sleep_ticks(&*frame),
        nr if dispatch_musl(frame, nr).is_some() => dispatch_musl(frame, nr).unwrap(),
        _ => {
            crate::kprintln!("[SYSCALL] unknown syscall nr={}", nr);
            ENOSYS
        }
    }
}

// ---------------------------------------------------------------------------
// Syscall Implementations (Core)
// ---------------------------------------------------------------------------

/// sys_write(0) — raw write to the serial port.
unsafe fn sys_write(frame: &SyscallFrame) -> u64 {
    let buf = frame.rdi as *const u8;
    let len = frame.rsi as usize;

    // Cap at 4 KiB to prevent serial flooding.
    if len == 0 || len > 4096 {
        return u64::MAX; // -EINVAL
    }

    // Safety: User pointers are currently trusted. Full VMM integration
    // will require `validate_user_ptr()` here.
    let slice = core::slice::from_raw_parts(buf, len);

    let mut written: usize = 0;
    for &byte in slice {
        serial_write_byte(byte);
        written += 1;
    }

    written as u64
}

/// sys_exit(1) — terminate the calling task.
unsafe fn sys_exit(frame: &SyscallFrame) -> u64 {
    let code = frame.rdi;
    crate::kprintln!("[SYSCALL] sys_exit({})", code);

    if let Some(t) = crate::scheduler::SCHEDULER.current_task_mut() {
        t.state = crate::scheduler::TaskState::Dead;
    }

    crate::scheduler::schedule();

    loop {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

/// sys_yield(2) — voluntarily give up the CPU time slice.
unsafe fn sys_yield() -> u64 {
    crate::scheduler::schedule();
    0
}

/// sys_getpid(3) — return the current task's ID.
unsafe fn sys_getpid() -> u64 {
    crate::scheduler::SCHEDULER
        .current_task_mut()
        .map(|t| t.id)
        .unwrap_or(0)
}

/// sys_sleep_ticks(4) — busy-wait for `n` scheduler ticks.
unsafe fn sys_sleep_ticks(frame: &SyscallFrame) -> u64 {
    let n = frame.rdi;
    let start = crate::scheduler::SCHEDULER.ticks();
    while crate::scheduler::SCHEDULER.ticks() < start + n {
        crate::scheduler::schedule();
    }
    0
}

fn serial_write_byte(byte: u8) {
    use core::fmt::Write as _;
    unsafe {
        let _ = crate::serial::SERIAL.write_char(byte as char);
    }
}

// ---------------------------------------------------------------------------
// User-Mode Transition
// ---------------------------------------------------------------------------

pub unsafe fn jump_to_usermode(entry: u64, user_stack: u64) -> ! {
    use crate::gdt::{UCODE_SELECTOR, UDATA_SELECTOR};

    let rflags: u64 = RFLAGS_IF;

    core::arch::asm!(
        "push {uds}",
        "push {usp}",
        "push {rfl}",
        "push {ucs}",
        "push {rip}",
        "iretq",
        uds = in(reg) UDATA_SELECTOR as u64,
        usp = in(reg) user_stack,
        rfl = in(reg) rflags,
        ucs = in(reg) UCODE_SELECTOR as u64,
        rip = in(reg) entry,
        options(noreturn),
    );
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

pub unsafe fn init() {
    Efer::update(|flags| *flags |= EferFlags::SYSTEM_CALL_EXTENSIONS);

    let star_value: u64 = ((STAR_USER_BASE as u64) << 48) | ((KCODE_SELECTOR as u64) << 32);

    let mut msr_star = Msr::new(MSR_STAR);
    msr_star.write(star_value);

    let mut msr_lstar = Msr::new(MSR_LSTAR);
    msr_lstar.write(syscall_entry as *const () as u64);

    let mut msr_fmask = Msr::new(MSR_FMASK);
    msr_fmask.write(RFLAGS_IF);

    crate::kprintln!("[SYSCALL] MSRs configured:");
    crate::kprintln!(
        "  STAR  = {:#018x}  (kCS={:#x} user_base={:#x})",
        star_value,
        KCODE_SELECTOR,
        STAR_USER_BASE,
    );
    crate::kprintln!(
        "  LSTAR = {:#018x}  (syscall_entry)",
        syscall_entry as *const () as u64
    );
    crate::kprintln!("  FMASK = {:#018x}  (IF cleared on entry)", RFLAGS_IF);
    crate::kprintln!("[SYSCALL] ready — ring-3 tasks can now use `syscall`.");
}

// ===========================================================================
// Musl POSIX Extension — Brick 10
// ===========================================================================
//
// These syscalls provide the backing required by standard C libraries (like
// musl) to operate. They bridge POSIX expectations with our custom VFS and PMM.

// ---------------------------------------------------------------------------
// File Descriptor Table
// ---------------------------------------------------------------------------

const MAX_FD: usize = 64;

/// Represents an active file handle for a task.
struct OpenFile {
    path: [u8; crate::vfs::PATH_MAX],
    path_len: usize,
    offset: u64,
    writable: bool,
    valid: bool,
}

impl OpenFile {
    const fn new() -> Self {
        Self {
            path: [0u8; crate::vfs::PATH_MAX],
            path_len: 0,
            offset: 0,
            writable: false,
            valid: false,
        }
    }

    fn path_str(&self) -> &str {
        core::str::from_utf8(&self.path[..self.path_len]).unwrap_or("")
    }
}

/// Global file descriptor table.
/// FDs 0, 1, and 2 are reserved for standard IO (stdin, stdout, stderr).
static mut FD_TABLE: [OpenFile; MAX_FD] = [const { OpenFile::new() }; MAX_FD];

/// Allocates the next available FD slot (>= 3).
unsafe fn alloc_fd() -> Option<usize> {
    (3..MAX_FD).find(|&i| !FD_TABLE[i].valid)
}

// ---------------------------------------------------------------------------
// POSIX Syscall Implementations
// ---------------------------------------------------------------------------

/// sys_open — opens a file in the VFS and returns a File Descriptor.
pub unsafe fn sys_open_impl(frame: &SyscallFrame) -> u64 {
    let path_ptr = frame.rdi as *const u8;
    let flags = frame.rsi as u32;
    let _mode = frame.rdx as u16;

    let mut path_buf = [0u8; crate::vfs::PATH_MAX];
    let mut len = 0usize;
    while len < crate::vfs::PATH_MAX - 1 {
        let b = core::ptr::read_volatile(path_ptr.add(len));
        if b == 0 {
            break;
        }
        path_buf[len] = b;
        len += 1;
    }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    let writable = flags & 0x3 != 0; // O_WRONLY or O_RDWR
    let o_creat = flags & 0x40 != 0;
    let o_trunc = flags & 0x200 != 0;

    if o_creat
        && crate::vfs::stat(path).is_none()
        && crate::vfs::create(path, crate::vfs::InodeKind::File, _mode).is_err()
    {
        return u64::MAX; // ENOENT
    }

    if crate::vfs::stat(path).is_none() {
        return u64::MAX; // ENOENT
    }

    if o_trunc && writable {
        crate::vfs::truncate(path, 0).ok();
    }

    let fd = match alloc_fd() {
        Some(fd) => fd,
        None => return u64::MAX, // EMFILE
    };

    let file = &mut FD_TABLE[fd];
    file.path[..len].copy_from_slice(&path_buf[..len]);
    file.path_len = len;
    file.offset = if o_trunc {
        0
    } else {
        crate::vfs::stat(path).map(|m| m.size).unwrap_or(0)
    };
    file.writable = writable;
    file.valid = true;

    fd as u64
}

/// sys_close — closes an open file descriptor.
pub unsafe fn sys_close_impl(frame: &SyscallFrame) -> u64 {
    let fd = frame.rdi as usize;
    if !(3..MAX_FD).contains(&fd) {
        return u64::MAX;
    }
    FD_TABLE[fd] = OpenFile::new();
    0
}

/// sys_read_fd — reads bytes from an open file descriptor.
pub unsafe fn sys_read_fd_impl(frame: &SyscallFrame) -> u64 {
    let fd = frame.rdi as usize;
    let buf = frame.rsi as *mut u8;
    let count = frame.rdx as usize;

    // FDs 0 intercepts standard input (keyboard)
    if fd == 0 {
        let ch = crate::drivers::keyboard::read_char();
        core::ptr::write_volatile(buf, ch);
        return 1;
    }

    if !(3..MAX_FD).contains(&fd) || !FD_TABLE[fd].valid {
        return u64::MAX; // EBADF
    }

    let file = &mut FD_TABLE[fd];
    let path = file.path_str();

    let mut tmp = [0u8; 4096];
    let to_read = count.min(4096);

    match crate::vfs::read(path, file.offset, &mut tmp[..to_read]) {
        Ok(0) => 0,
        Ok(n) => {
            core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf, n);
            file.offset += n as u64;
            n as u64
        }
        Err(_) => u64::MAX,
    }
}

/// sys_write_fd — writes bytes to an open file descriptor.
pub unsafe fn sys_write_fd_impl(frame: &SyscallFrame) -> u64 {
    let fd = frame.rdi as usize;
    let buf = frame.rsi as *const u8;
    let count = frame.rdx as usize;

    // FDs 1 & 2 intercept standard output/error (serial port)
    if fd == 1 || fd == 2 {
        return sys_write(frame);
    }

    if !(3..MAX_FD).contains(&fd) || !FD_TABLE[fd].valid || !FD_TABLE[fd].writable {
        return u64::MAX; // EBADF or EACCES
    }

    let file = &mut FD_TABLE[fd];
    let path = {
        let mut tmp = [0u8; crate::vfs::PATH_MAX];
        tmp[..file.path_len].copy_from_slice(&file.path[..file.path_len]);
        tmp
    };
    let path_len = file.path_len;
    let offset = file.offset;
    let path_str = core::str::from_utf8(&path[..path_len]).unwrap_or("");

    let slice = core::slice::from_raw_parts(buf, count);
    match crate::vfs::write(path_str, offset, slice) {
        Ok(n) => {
            FD_TABLE[fd].offset += n as u64;
            n as u64
        }
        Err(_) => u64::MAX,
    }
}

/// sys_lseek — repositions the read/write file offset.
pub unsafe fn sys_lseek_impl(frame: &SyscallFrame) -> u64 {
    let fd = frame.rdi as usize;
    let offset = frame.rsi as i64;
    let whence = frame.rdx as u32;

    if !(3..MAX_FD).contains(&fd) || !FD_TABLE[fd].valid {
        return u64::MAX;
    }

    let file = &mut FD_TABLE[fd];
    let path_str = core::str::from_utf8(&file.path[..file.path_len]).unwrap_or("");
    let file_size = crate::vfs::stat(path_str).map(|m| m.size).unwrap_or(0);

    let new_offset: i64 = match whence {
        0 => offset,                      // SEEK_SET
        1 => file.offset as i64 + offset, // SEEK_CUR
        2 => file_size as i64 + offset,   // SEEK_END
        _ => return u64::MAX,
    };

    if new_offset < 0 {
        return u64::MAX;
    }
    file.offset = new_offset as u64;
    file.offset
}

/// sys_brk — expands or queries the data segment (user heap).
///
/// Note: A full implementation requires VMM page table adjustments.
/// This stub provides a simple monotonic bump allocator model for basic tests.
static mut USER_BRK: u64 = 0x0000_7FFF_0000_0000;

pub unsafe fn sys_brk_impl(frame: &SyscallFrame) -> u64 {
    let addr = frame.rdi;
    if addr == 0 {
        return USER_BRK;
    }
    if addr > USER_BRK {
        USER_BRK = addr;
    }
    USER_BRK
}

/// sys_mmap — maps memory into the task's address space.
///
/// Current implementation allocates physical frames via PMM and returns
/// direct-mapped pointers, supporting `MAP_ANONYMOUS` requests from standard allocators.
pub unsafe fn sys_mmap_impl(frame: &SyscallFrame) -> u64 {
    let len = frame.rsi as usize;
    let flags = frame.r10 as u32;

    // MAP_ANONYMOUS = 0x20
    if flags & 0x20 == 0 {
        return u64::MAX;
    } // File-backed mapping not supported

    let n_frames = len.div_ceil(crate::pmm::FRAME_SIZE as usize);

    match crate::pmm::alloc_frames_contiguous(n_frames) {
        Some(phys) => {
            let virt = crate::vmm::phys_to_virt(x86_64::PhysAddr::new(phys));
            core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0u8, len); // Zero out anonymous memory
            virt.as_u64()
        }
        None => u64::MAX, // ENOMEM
    }
}

/// sys_munmap — unmaps memory.
pub unsafe fn sys_munmap_impl(_frame: &SyscallFrame) -> u64 {
    // Fictional success. In production, this must free PMM frames and clear page tables.
    0
}

// ---------------------------------------------------------------------------
// Extended Dispatcher
// ---------------------------------------------------------------------------

/// Extended POSIX dispatcher — invoked within `syscall_dispatch()`.
/// Returns `None` if the syscall number is unhandled.
pub unsafe fn dispatch_musl(frame: *mut SyscallFrame, nr: u64) -> Option<u64> {
    let f = &*frame;
    match nr {
        SYS_OPEN => Some(sys_open_impl(f)),
        SYS_CLOSE => Some(sys_close_impl(f)),
        SYS_READ_FD => Some(sys_read_fd_impl(f)),
        SYS_WRITE_FD => Some(sys_write_fd_impl(f)),
        SYS_LSEEK => Some(sys_lseek_impl(f)),
        SYS_BRK => Some(sys_brk_impl(f)),
        SYS_MMAP => Some(sys_mmap_impl(f)),
        SYS_MUNMAP => Some(sys_munmap_impl(f)),
        _ => None,
    }
}
