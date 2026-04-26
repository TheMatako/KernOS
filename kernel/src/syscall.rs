// kernel/src/syscall.rs
//
// System call interface — Brick 6.
//
// ── Mechanism: syscall / sysret ───────────────────────────────────────────────
//
// On x86_64, user-mode code executes `syscall` to enter the kernel.  This is
// faster than the old `int 0x80` approach because it skips the IDT lookup and
// the privilege-level check done by the interrupt gate.
//
// The CPU does the following on `syscall`:
//   1. Saves RIP  → RCX  (return address for sysret)
//   2. Saves RFLAGS → R11
//   3. Clears RFLAGS bits listed in IA32_FMASK (we clear IF to mask interrupts)
//   4. Loads CS  from STAR[47:32]     (kernel code selector)
//   5. Loads SS  from STAR[47:32] + 8 (kernel data selector)
//   6. Jumps to  LSTAR                (our syscall entry point)
//
// On `sysretq` (64-bit return):
//   1. Restores RIP    from RCX
//   2. Restores RFLAGS from R11
//   3. Loads CS from STAR[63:48] + 16 (user code selector)
//   4. Loads SS from STAR[63:48] + 8  (user data selector)
//   5. Drops to ring 3
//
// The selectors are chosen in gdt.rs to satisfy these equations.
//
// ── MSRs used ────────────────────────────────────────────────────────────────
//
//   IA32_STAR   (0xC000_0081) — segment selectors for syscall/sysret
//   IA32_LSTAR  (0xC000_0082) — 64-bit syscall entry point (RIP)
//   IA32_FMASK  (0xC000_0084) — RFLAGS bits to clear on syscall
//   IA32_EFER   (0xC000_0080) — Extended Feature Enable Register (SCE bit)
//
// ── Syscall ABI ──────────────────────────────────────────────────────────────
//
// We follow the Linux x86_64 syscall ABI so that a future musl libc port
// (Brick 10) requires no changes on the user side:
//
//   RAX  = syscall number (on entry) / return value (on exit)
//   RDI  = argument 1
//   RSI  = argument 2
//   RDX  = argument 3
//   R10  = argument 4   (note: NOT RCX — RCX is clobbered by the CPU)
//   R8   = argument 5
//   R9   = argument 6
//
// ── Register save/restore ─────────────────────────────────────────────────────
//
// The `syscall` entry stub (in global_asm!) saves all caller-saved and
// syscall-clobbered registers onto the kernel stack, then calls the Rust
// dispatcher.  On return it restores them and executes `sysretq`.
//
// ── Syscall table ────────────────────────────────────────────────────────────
//
//   0  sys_write(buf: *const u8, len: usize) → usize   — write to serial port
//   1  sys_exit(code: u64) → !                          — terminate task
//   2  sys_yield() → 0                                  — voluntarily yield CPU
//   3  sys_getpid() → u64                               — return current TID
//   4  sys_sleep_ticks(n: u64) → 0                      — busy-wait n ticks

#![allow(dead_code)]
#![allow(static_mut_refs)]

use crate::gdt::{KCODE_SELECTOR, STAR_USER_BASE};
use x86_64::registers::model_specific::{Efer, EferFlags, Msr};

// ---------------------------------------------------------------------------
// MSR addresses (Intel SDM Vol. 4)
// ---------------------------------------------------------------------------

/// IA32_EFER — Extended Feature Enable Register.
/// Bit 0 (SCE) must be set to enable the `syscall`/`sysret` instructions.
const MSR_EFER: u32 = 0xC000_0080;

/// IA32_STAR — Syscall Target Address Register.
/// bits [63:48] = user segment base (for sysret)
/// bits [47:32] = kernel CS (for syscall)
/// bits [31:0]  = legacy 32-bit EIP (ignored in 64-bit mode)
const MSR_STAR: u32 = 0xC000_0081;

/// IA32_LSTAR — Long Mode STAR — 64-bit syscall entry point.
const MSR_LSTAR: u32 = 0xC000_0082;

/// IA32_FMASK — RFLAGS mask applied on syscall entry.
/// Every bit set here is cleared in RFLAGS when syscall fires.
/// We clear IF (bit 9) so the kernel is not preempted during the syscall stub.
const MSR_FMASK: u32 = 0xC000_0084;

// ---------------------------------------------------------------------------
// RFLAGS bits
// ---------------------------------------------------------------------------

/// Interrupt Flag — bit 9 of RFLAGS.
const RFLAGS_IF: u64 = 1 << 9;

// ---------------------------------------------------------------------------
// Syscall numbers
// ---------------------------------------------------------------------------

pub const SYS_WRITE: u64 = 0;
pub const SYS_EXIT: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_SLEEP_TICKS: u64 = 4;

/// Return value indicating an unknown / unimplemented syscall.
/// Matches the Linux convention of returning -ENOSYS as a large u64.
const ENOSYS: u64 = u64::MAX - 38 + 1; // -38 two's complement

// ---------------------------------------------------------------------------
// Saved register frame
// ---------------------------------------------------------------------------

/// All registers pushed by the syscall entry stub.
///
/// `#[repr(C)]` is mandatory: the assembly stub pushes registers in the exact
/// order of this struct's fields (top = first pushed = last in memory because
/// stacks grow down).
///
/// The Rust dispatcher receives a `*mut SyscallFrame` so it can inspect (and
/// if needed modify) the saved state — useful for, e.g., modifying RIP to
/// restart a syscall.
#[repr(C)]
pub struct SyscallFrame {
    // Fields are in the order they appear on the stack after all pushes.
    // The stub pushes: r11, r10, r9, r8, rsi, rdi, rdx, rcx(=saved rip),
    //                  rbx, rbp, r12, r13, r14, r15
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    /// RCX holds the user RIP saved by the `syscall` instruction.
    pub rcx: u64,
    pub rdx: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    /// R11 holds the user RFLAGS saved by the `syscall` instruction.
    pub r11: u64,
    /// Syscall number (RAX on entry; becomes return value on exit).
    pub rax: u64,
}

// ---------------------------------------------------------------------------
// Syscall entry stub (assembly)
// ---------------------------------------------------------------------------

// The stub is a naked function — the compiler must emit *only* our assembly,
// with no prologue/epilogue, because the stack layout must be exact.
//
// On entry (set by the CPU via `syscall`):
//   RAX = syscall number
//   RDI, RSI, RDX, R10, R8, R9 = arguments
//   RCX = saved user RIP
//   R11 = saved user RFLAGS
//   CS/SS = kernel selectors (from STAR)
//   IF   = 0 (cleared by FMASK)
//
// What we do:
//   1. Push all registers onto the kernel stack (building SyscallFrame).
//   2. Call `syscall_dispatch(frame: *mut SyscallFrame, nr: u64)`.
//   3. The return value (RAX) from dispatch is already in RAX.
//   4. Pop all registers.
//   5. `sysretq` → restores RIP from RCX, RFLAGS from R11, drops to ring 3.
core::arch::global_asm!(
    ".global syscall_entry",
    ".type syscall_entry, @function",
    "syscall_entry:",
    // ── Save all non-scratch registers + syscall-clobbered regs ──────────────
    // Push order (high → low address, i.e. last pushed = lowest on stack):
    "push rax", // syscall number (will become return value)
    "push r11", // saved user RFLAGS
    "push r10", // arg4
    "push r9",  // arg5
    "push r8",  // arg6 (wait — r8 is arg5, r9 is arg6... keeping Linux order)
    "push rsi", // arg2
    "push rdi", // arg1
    "push rdx", // arg3
    "push rcx", // saved user RIP
    "push rbx", // callee-saved
    "push rbp", // callee-saved
    "push r12", // callee-saved
    "push r13", // callee-saved
    "push r14", // callee-saved
    "push r15", // callee-saved  ← RSP now points here
    // ── Call the Rust dispatcher ───────────────────────────────────────────────
    // Argument 1 (RDI) = pointer to the SyscallFrame on the stack.
    // Argument 2 (RSI) = syscall number (was RAX on entry; now at frame.rax).
    "mov rdi, rsp",          // &SyscallFrame
    "mov rsi, [rsp + 14*8]", // frame.rax = syscall number (14 u64s from rsp)
    "call syscall_dispatch",
    // RAX now holds the return value from syscall_dispatch.
    // Store it into frame.rax so sysretq restores it correctly.
    "mov [rsp + 14*8], rax",
    // ── Restore all registers ─────────────────────────────────────────────────
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "pop rcx", // user RIP (for sysretq)
    "pop rdx",
    "pop rdi",
    "pop rsi",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11", // user RFLAGS (for sysretq)
    "pop rax", // return value
    // ── Return to user mode ───────────────────────────────────────────────────
    // `sysretq` restores RIP from RCX, RFLAGS from R11,
    // and sets CS/SS to the user selectors (from STAR[63:48]).
    "sysretq",
);

extern "C" {
    fn syscall_entry();
}

// ---------------------------------------------------------------------------
// Rust syscall dispatcher
// ---------------------------------------------------------------------------

/// Called from the `syscall_entry` stub with a pointer to the saved register
/// frame and the syscall number.
///
/// Returns the value that will be placed in RAX when execution returns to user
/// mode.  A return value of `u64::MAX - 38 + 1` means "not implemented"
/// (mirrors Linux's -ENOSYS in two's-complement u64).
///
/// # Safety
/// Called from assembly with a raw pointer to the kernel stack.  `frame` is
/// valid for the lifetime of this call.
#[no_mangle]
pub unsafe extern "C" fn syscall_dispatch(frame: *mut SyscallFrame, nr: u64) -> u64 {
    match nr {
        SYS_WRITE => sys_write(&*frame),
        SYS_EXIT => sys_exit(&*frame),
        SYS_YIELD => sys_yield(),
        SYS_GETPID => sys_getpid(),
        SYS_SLEEP_TICKS => sys_sleep_ticks(&*frame),
        _ => {
            crate::kprintln!("[SYSCALL] unknown syscall nr={}", nr);
            ENOSYS
        }
    }
}

// ---------------------------------------------------------------------------
// Syscall implementations
// ---------------------------------------------------------------------------

/// sys_write(0) — write bytes from a user buffer to the serial port.
///
/// Arguments (from SyscallFrame):
///   RDI = buf: *const u8  — pointer to user-space buffer
///   RSI = len: usize      — number of bytes to write
///
/// Returns: number of bytes written, or u64::MAX on error.
///
/// Security note: at this stage we do *no* address validation on `buf`.
/// Brick 7 (VMM user mappings) must add a `validate_user_ptr()` check here.
unsafe fn sys_write(frame: &SyscallFrame) -> u64 {
    let buf = frame.rdi as *const u8;
    let len = frame.rsi as usize;

    // Sanity cap: refuse writes larger than 4 KiB to avoid serial flooding.
    if len == 0 || len > 4096 {
        return u64::MAX; // -EINVAL
    }

    // Safety: we trust the user pointer for now (no MMU validation yet).
    let slice = core::slice::from_raw_parts(buf, len);

    let mut written: usize = 0;
    for &byte in slice {
        // Route through the serial driver's write_byte.
        // We access SERIAL indirectly through the _print path.
        // For individual bytes we call a helper below.
        serial_write_byte(byte);
        written += 1;
    }

    written as u64
}

/// sys_exit(1) — terminate the calling task.
///
/// Arguments:
///   RDI = exit code (logged but otherwise ignored at this stage)
///
/// Does not return to user mode.
unsafe fn sys_exit(frame: &SyscallFrame) -> u64 {
    let code = frame.rdi;
    crate::kprintln!("[SYSCALL] sys_exit({})", code);

    // Mark the current task as Dead so the scheduler skips it.
    if let Some(t) = crate::scheduler::SCHEDULER.current_task_mut() {
        t.state = crate::scheduler::TaskState::Dead;
    }

    // Yield immediately — we will not be rescheduled.
    crate::scheduler::schedule();

    // Unreachable in normal operation; the loop satisfies `-> u64`.
    loop {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

/// sys_yield(2) — voluntarily give up the CPU.
///
/// Returns 0.
unsafe fn sys_yield() -> u64 {
    crate::scheduler::schedule();
    0
}

/// sys_getpid(3) — return the current task's ID.
///
/// Returns the TID as a u64.
unsafe fn sys_getpid() -> u64 {
    crate::scheduler::SCHEDULER
        .current_task_mut()
        .map(|t| t.id)
        .unwrap_or(0)
}

/// sys_sleep_ticks(4) — busy-wait for `n` scheduler ticks.
///
/// Arguments:
///   RDI = n: u64 — number of ticks to sleep
///
/// This is a simple spin-based sleep; a real implementation would block the
/// task and add it to a timer queue (Brick 7+).
///
/// Returns 0.
unsafe fn sys_sleep_ticks(frame: &SyscallFrame) -> u64 {
    let n = frame.rdi;
    let start = crate::scheduler::SCHEDULER.ticks();
    while crate::scheduler::SCHEDULER.ticks() < start + n {
        crate::scheduler::schedule();
    }
    0
}

// ---------------------------------------------------------------------------
// Serial byte helper (private)
// ---------------------------------------------------------------------------

/// Writes one byte to the serial port.
///
/// This bypasses `kprint!` to avoid the formatting overhead inside a syscall.
///
/// # Safety
/// Calls into serial::_print via format_args, which accesses static mut SERIAL.
/// Safe in a single-threaded kernel.
fn serial_write_byte(byte: u8) {
    // We route through the existing fmt::Write path.
    use core::fmt::Write as _;
    // Safety: single-threaded; SERIAL is initialised before any syscall fires.
    unsafe {
        let _ = crate::serial::SERIAL.write_char(byte as char);
    }
}

// ---------------------------------------------------------------------------
// User-mode entry
// ---------------------------------------------------------------------------

/// Switches from ring 0 to ring 3 and begins executing `entry`.
///
/// Uses `iretq` which pops:
///   SS, RSP, RFLAGS, CS, RIP
/// from the kernel stack and drops to ring 3.
///
/// # Arguments
/// * `entry`      — virtual address of the user-mode entry point.
/// * `user_stack` — virtual address of the top of the user-mode stack
///   (must be 16-byte aligned, already mapped in the user page tables).
///
/// # Safety
/// - `entry` and `user_stack` must be valid ring-3 virtual addresses.
/// - The user page tables must be loaded (CR3 must reflect the user mapping).
/// - Interrupts must be disabled before calling; they will be re-enabled by
///   RFLAGS restoration (we set IF in the pushed RFLAGS).
/// - Never returns to ring 0 (unless the user calls `syscall`).
pub unsafe fn jump_to_usermode(entry: u64, user_stack: u64) -> ! {
    use crate::gdt::{UCODE_SELECTOR, UDATA_SELECTOR};

    // RFLAGS value for user mode: interrupts enabled (IF=1), IOPL=0.
    let rflags: u64 = RFLAGS_IF;

    // Build the iretq frame on the kernel stack (from top to bottom):
    //   [SS, RSP, RFLAGS, CS, RIP]
    //
    // We use a raw asm block that pushes these five values and then executes
    // `iretq`.  The `in(reg)` operands are moved into temporary registers by
    // the compiler; we then push them in the right order.
    core::arch::asm!(
        // Push SS (user data selector with RPL=3).
        "push {uds}",
        // Push RSP (user stack pointer).
        "push {usp}",
        // Push RFLAGS (IF=1).
        "push {rfl}",
        // Push CS (user code selector with RPL=3).
        "push {ucs}",
        // Push RIP (entry point).
        "push {rip}",
        // iretq: pops RIP, CS, RFLAGS, RSP, SS and drops to ring 3.
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
// Initialisation
// ---------------------------------------------------------------------------

/// Configures the MSRs required for `syscall`/`sysret` and enables SCE in EFER.
///
/// Call order in `kernel_main` (after gdt::init):
///   gdt → idt → pmm → vmm → slab → apic → scheduler → **syscall::init()** → sti
///
/// # Safety
/// Writes to MSRs — privileged ring-0 operation.
/// Must be called exactly once, with interrupts disabled.
pub unsafe fn init() {
    // ── 1. Enable SCE (SysCall Enable) in IA32_EFER ───────────────────────────
    //
    // EFER.SCE (bit 0) must be set before the CPU will accept `syscall`.
    // The x86_64 crate's `Efer::update` reads-modifies-writes safely.
    Efer::update(|flags| *flags |= EferFlags::SYSTEM_CALL_EXTENSIONS);

    // ── 2. Write IA32_STAR ────────────────────────────────────────────────────
    //
    // bits [63:48] = STAR_USER_BASE = 0x13
    //   → sysret SS = 0x13 + 8  = 0x1B (user data, RPL=3) ✓
    //   → sysret CS = 0x13 + 16 = 0x23 (user code, RPL=3) ✓
    //
    // bits [47:32] = KCODE_SELECTOR = 0x08
    //   → syscall CS = 0x08 (kernel code)              ✓
    //   → syscall SS = 0x08 + 8 = 0x10 (kernel data)  ✓
    //
    // bits [31:0] are the legacy 32-bit SYSCALL EIP — ignored in 64-bit mode.
    let star_value: u64 = ((STAR_USER_BASE as u64) << 48) | ((KCODE_SELECTOR as u64) << 32);

    let mut msr_star = Msr::new(MSR_STAR);
    msr_star.write(star_value);

    // ── 3. Write IA32_LSTAR — our assembly entry point ────────────────────────
    let mut msr_lstar = Msr::new(MSR_LSTAR);
    msr_lstar.write(syscall_entry as *const () as u64);

    // ── 4. Write IA32_FMASK — clear IF on syscall entry ──────────────────────
    //
    // Clearing IF prevents the APIC timer from preempting the kernel during the
    // syscall save/restore stub.  We re-enable interrupts manually at the end
    // of long-running syscalls if needed (not yet — Brick 7+).
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
