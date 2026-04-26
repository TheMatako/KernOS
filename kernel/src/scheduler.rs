// kernel/src/scheduler.rs
//
// Preemptive round-robin scheduler — Brick 5.
//
// ── Design overview ───────────────────────────────────────────────────────────
//
// Tasks
// ─────
// A `Task` is the kernel's unit of execution.  At this stage every task runs
// entirely in ring 0 (kernel mode).  User-mode tasks come in Brick 6 (syscall).
//
// Each task owns a private kernel stack (allocated from the slab allocator or
// directly from the PMM).  When a task is not running, its CPU state is saved
// on that stack and the stack pointer is stored in `Task::rsp`.
//
// Context switch
// ──────────────
// Switching from task A to task B means:
//   1. Push callee-saved registers onto A's stack.
//   2. Save A's RSP in `A.rsp`.
//   3. Load B's RSP from `B.rsp`.
//   4. Pop callee-saved registers from B's stack.
//   5. `ret` — which pops the saved RIP from B's stack, resuming B where it
//      left off (or jumping to B's entry function if it has never run).
//
// Only callee-saved registers need explicit save/restore because the Rust/C
// calling convention already guarantees that callee-saved registers are
// preserved across function calls.  Caller-saved registers (RAX, RCX, RDX,
// RSI, RDI, R8–R11) are irrelevant: the task already saved them before making
// the call that led to the context switch.
//
// Callee-saved on x86_64 System V ABI: RBX, RBP, R12, R13, R14, R15.
// Plus RSP (saved explicitly in Task::rsp).
//
// New task stack layout (from high address, RSP → low address):
// ┌──────────────────────────────┐ ← top of stack (high address)
// │  task_entry  (8 bytes)       │ ← `ret` in switch_context jumps here
// │  0 for R15   (8 bytes)       │
// │  0 for R14   (8 bytes)       │
// │  0 for R13   (8 bytes)       │
// │  0 for R12   (8 bytes)       │
// │  0 for RBP   (8 bytes)       │
// │  0 for RBX   (8 bytes)       │ ← initial RSP (switch_context pops from here)
// └──────────────────────────────┘
//
// Run queue
// ─────────
// A fixed-size circular array of `*mut Task` pointers.
// `schedule()` picks the next `Ready` task in round-robin order.
// The `Idle` task (task 0) runs when no other task is ready.
//
// Preemption
// ──────────
// The APIC timer fires at ~100 Hz.  The IDT handler calls `scheduler::tick()`,
// which calls `schedule()` to pick the next task and performs the context switch.

#![allow(static_mut_refs)]
#![allow(dead_code)]

use crate::pmm;
use core::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Kernel stack size per task: 16 KiB.
///
/// Large enough for deep call stacks (e.g. a panic inside a handler) but small
/// enough to allocate many tasks.  Must be a multiple of 4 KiB (PMM granularity).
const TASK_STACK_SIZE: usize = 4096 * 4; // 16 KiB

/// Maximum number of tasks the run queue can hold (including idle).
/// Increase for more concurrency; each slot is just a pointer (8 bytes).
const MAX_TASKS: usize = 64;

// ---------------------------------------------------------------------------
// Task ID
// ---------------------------------------------------------------------------

/// Global monotonically-increasing task ID counter.
static NEXT_TID: AtomicU64 = AtomicU64::new(0);

/// Allocates the next unique task ID.
fn next_tid() -> u64 {
    NEXT_TID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Task state
// ---------------------------------------------------------------------------

/// The lifecycle state of a kernel task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TaskState {
    /// The task is eligible to run.
    Ready,
    /// The task is currently executing on the CPU.
    Running,
    /// The task is waiting for an event (timer, I/O, …).
    /// Not yet used — placeholder for Brick 6+.
    Blocked,
    /// The task has returned from its entry function and will not run again.
    Dead,
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

/// One kernel task (thread).
///
/// `#[repr(C)]` is required because `switch_context` (assembly) accesses the
/// `rsp` field at a known offset.  We also keep the struct as flat as possible
/// to avoid surprises.
#[repr(C)]
pub struct Task {
    /// Saved kernel stack pointer.
    ///
    /// When the task is not running, RSP is stored here.
    /// When the task is running, the CPU uses the real RSP register.
    ///
    /// **Must be the very first field** — the assembly stub relies on offset 0.
    pub rsp: u64,

    /// Unique task identifier.
    pub id: u64,

    /// Human-readable name (for debug output).
    pub name: &'static str,

    /// Current lifecycle state.
    pub state: TaskState,

    /// Physical base address of this task's kernel stack.
    ///
    /// Stored so we can free it when the task dies (Brick 6+).
    stack_phys_base: u64,
}

impl Task {
    /// Creates a new task and sets up its initial stack frame.
    ///
    /// The task will begin execution at `entry` when first scheduled.
    ///
    /// `entry` must be a `fn() -> !` — tasks must never return.
    /// If a task returns, the CPU will execute whatever is below its stack,
    /// which will likely cause a triple fault.  We install a `task_exit` trampoline
    /// below the entry address to catch this case gracefully.
    ///
    /// # Safety
    /// Calls `pmm::alloc_frames_contiguous` to allocate the kernel stack.
    pub unsafe fn new(name: &'static str, entry: fn() -> !) -> Self {
        // ── Allocate kernel stack ─────────────────────────────────────────────
        let n_frames = TASK_STACK_SIZE / pmm::FRAME_SIZE as usize;
        let stack_phys = pmm::alloc_frames_contiguous(n_frames)
            .expect("scheduler: out of memory for task stack");

        // ── Compute stack top ─────────────────────────────────────────────────
        // x86_64 stacks grow downward.  The "top" is the highest address.
        let stack_top = stack_phys + TASK_STACK_SIZE as u64;

        // ── Write the initial stack frame ─────────────────────────────────────
        //
        // We push (from high to low):
        //   [task_exit trampoline] — catches accidental returns from entry
        //   [entry fn address]     — `ret` in switch_context jumps here
        //   [0; 6]                 — callee-saved regs (RBX, RBP, R12–R15)
        //
        // After writing, RSP points at the RBX slot.

        let mut sp = stack_top as *mut u64;

        // Helper: push one u64 onto the fake stack.
        macro_rules! push {
            ($val:expr) => {{
                sp = sp.sub(1);
                sp.write($val);
            }};
        }

        // Trampoline: if entry ever returns (it should not), we call task_exit.
        push!(task_exit as *const () as u64);

        // Entry function address — `ret` inside switch_context will jump here.
        push!(entry as *const () as u64);

        // Callee-saved registers (initialised to 0 for a new task).
        push!(0); // R15
        push!(0); // R14
        push!(0); // R13
        push!(0); // R12
        push!(0); // RBP
        push!(0); // RBX  ← initial RSP after switch_context pops these

        Task {
            rsp: sp as u64,
            id: next_tid(),
            name,
            state: TaskState::Ready,
            stack_phys_base: stack_phys,
        }
    }
}

/// Trampoline called if a task entry function returns.
///
/// Marks the task as Dead and yields.  This should never happen in a
/// well-written kernel; we print a warning and loop.
fn task_exit() -> ! {
    crate::kprintln!("[SCHED] WARNING: task returned — marking Dead");
    // Mark the current task as Dead.
    unsafe {
        if let Some(current) = SCHEDULER.current_task_mut() {
            current.state = TaskState::Dead;
        }
    }
    // Yield forever — the scheduler will skip Dead tasks.
    loop {
        schedule();
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

// ---------------------------------------------------------------------------
// Context switch (assembly)
// ---------------------------------------------------------------------------

// We implement the context switch as a naked function in inline assembly.
//
// Signature (System V AMD64 ABI):
//   switch_context(old_rsp: *mut u64, new_rsp: u64)
//                  ^^^^^^^^^^^^^^^^   ^^^^^^^^^^^^
//                  RDI                RSI
//
// What it does:
//   1. Push callee-saved registers onto the current (old task's) stack.
//   2. Save RSP into *old_rsp (RDI).
//   3. Load new_rsp (RSI) into RSP.
//   4. Pop callee-saved registers from the new stack.
//   5. ret → jumps to the new task's saved RIP.
//
// This function intentionally has no Rust body — the naked attribute means the
// compiler emits *only* our asm, with no prologue/epilogue.
//
// Safety: must only be called from `schedule()` with interrupts disabled.

core::arch::global_asm!(
    ".global switch_context",
    ".type switch_context, @function",
    "switch_context:",
    // Save callee-saved registers on the old task's stack.
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    // Save old RSP into *old_rsp (rdi = first argument).
    "mov [rdi], rsp",
    // Switch to the new stack (rsi = second argument).
    "mov rsp, rsi",
    // Restore callee-saved registers from the new task's stack.
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    // Return — pops the saved RIP from the new stack and jumps there.
    "ret",
);

extern "C" {
    /// Context switch from the task whose RSP is stored at `*old_rsp` to the
    /// task with stack pointer `new_rsp`.
    ///
    /// # Safety
    /// - Interrupts must be disabled before calling.
    /// - Both `*old_rsp` and `new_rsp` must point to valid task stacks.
    fn switch_context(old_rsp: *mut u64, new_rsp: u64);
}

// ---------------------------------------------------------------------------
// Run queue
// ---------------------------------------------------------------------------

/// The global scheduler state.
///
/// A single instance; the kernel is single-core at this stage.
pub struct Scheduler {
    /// Circular array of task pointers.
    ///
    /// We use raw pointers because the tasks are heap-allocated (or static)
    /// and we need stable addresses.  Indices are valid in `[0, len)`.
    tasks: [*mut Task; MAX_TASKS],

    /// Number of tasks registered (including idle).
    len: usize,

    /// Index of the currently running task in `tasks`.
    current: usize,

    /// Total scheduler ticks since boot.
    ticks: u64,
}

// Safety: the kernel is single-threaded at this stage; no concurrent access.
unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

impl Scheduler {
    const fn new() -> Self {
        Self {
            tasks: [core::ptr::null_mut(); MAX_TASKS],
            len: 0,
            current: 0,
            ticks: 0,
        }
    }

    /// Returns a mutable reference to the currently running task.
    pub unsafe fn current_task_mut(&mut self) -> Option<&mut Task> {
        if self.len == 0 {
            return None;
        }
        self.tasks[self.current].as_mut()
    }

    /// Registers a task with the scheduler.
    ///
    /// `task` must be a heap-allocated or static `Task` whose address is
    /// stable (will not move).
    ///
    /// # Panics
    /// Panics if `MAX_TASKS` is exceeded.
    pub fn add_task(&mut self, task: *mut Task) {
        assert!(self.len < MAX_TASKS, "scheduler: MAX_TASKS exceeded");
        self.tasks[self.len] = task;
        self.len += 1;
    }

    /// Picks the next Ready task in round-robin order and switches to it.
    ///
    /// Called from the APIC timer handler (`scheduler::tick()`).
    ///
    /// # Safety
    /// Must be called with interrupts disabled (the timer handler does this
    /// automatically — interrupts are masked during an interrupt handler).
    pub unsafe fn schedule(&mut self) {
        self.ticks += 1;

        if self.len == 0 {
            return; // Nothing to schedule yet.
        }

        // Mark the current task as Ready (it was Running).
        if let Some(t) = self.tasks[self.current].as_mut() {
            if t.state == TaskState::Running {
                t.state = TaskState::Ready;
            }
        }

        // Find the next Ready task in round-robin order.
        let start = self.current;
        let mut next = (start + 1) % self.len;
        loop {
            if let Some(t) = self.tasks[next].as_ref() {
                if t.state == TaskState::Ready {
                    break;
                }
            }
            next = (next + 1) % self.len;
            if next == start {
                // No Ready task found — run idle (index 0) as fallback.
                next = 0;
                break;
            }
        }

        if next == self.current {
            // Same task — no switch needed; just re-mark it Running.
            if let Some(t) = self.tasks[self.current].as_mut() {
                t.state = TaskState::Running;
            }
            return;
        }

        // Perform the actual context switch.
        let old_idx = self.current;
        self.current = next;

        let old_task = &mut *self.tasks[old_idx];
        let new_task = &mut *self.tasks[next];

        new_task.state = TaskState::Running;

        // Pointer to where we will save the old RSP.
        let old_rsp_ptr: *mut u64 = &mut old_task.rsp;
        let new_rsp: u64 = new_task.rsp;

        // This call saves old registers, swaps stacks, restores new registers,
        // and "returns" into the new task.
        switch_context(old_rsp_ptr, new_rsp);

        // Execution resumes here when *this* task is scheduled in again.
    }

    /// Returns the total number of scheduler ticks since boot.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }
}

// ---------------------------------------------------------------------------
// Global scheduler instance
// ---------------------------------------------------------------------------

/// The one global scheduler.
///
/// Accessed from both the timer interrupt handler and `kernel_main`.
/// Single-threaded → no lock needed at this stage.
pub static mut SCHEDULER: Scheduler = Scheduler::new();

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialises the scheduler and registers the idle task.
///
/// The idle task simply loops on `hlt`.  It is always Ready (never blocks)
/// and serves as the fallback when no other task is ready.
///
/// # Safety
/// Writes to `static mut` globals.  Call once before enabling interrupts.
pub unsafe fn init() {
    // Allocate the idle task.
    // We use `Box`-less raw allocation because `alloc` is not set up yet;
    // instead we carve space out of a static.
    static mut IDLE_TASK: core::mem::MaybeUninit<Task> = core::mem::MaybeUninit::uninit();

    IDLE_TASK.write(Task::new("idle", idle_task));
    let idle_ptr = IDLE_TASK.as_mut_ptr();

    // The idle task starts as Running (we are currently the idle context).
    (*idle_ptr).state = TaskState::Running;

    SCHEDULER.add_task(idle_ptr);

    crate::kprintln!(
        "[SCHED] scheduler init — idle task registered (tid={})",
        (*idle_ptr).id
    );
}

/// Spawns a new task and adds it to the run queue.
///
/// `entry` must be a `fn() -> !` — tasks must never return.
///
/// Returns the task ID.
///
/// # Safety
/// Allocates kernel memory (PMM + slab).  Must be called after `slab::init()`.
pub unsafe fn spawn(name: &'static str, entry: fn() -> !) -> u64 {
    // Allocate a Task struct from the slab allocator.
    let ptr = crate::slab::kmalloc(core::mem::size_of::<Task>())
        .expect("spawn: kmalloc for Task failed") as *mut Task;

    // Initialise the task (sets up the fake initial stack frame).
    ptr.write(Task::new(name, entry));

    let tid = (*ptr).id;
    SCHEDULER.add_task(ptr);

    crate::kprintln!("[SCHED] spawned task '{}' (tid={})", name, tid);
    tid
}

/// Called by the APIC timer interrupt handler every tick (~100 Hz).
///
/// Increments the tick counter and triggers a context switch.
///
/// # Safety
/// Called from an interrupt handler — interrupts are already masked.
pub unsafe fn tick() {
    SCHEDULER.schedule();
}

/// Voluntarily yields the CPU to the next ready task.
///
/// Can be called from anywhere in the kernel (with interrupts enabled or not).
/// We briefly disable interrupts to safely call `schedule()`.
pub fn schedule() {
    unsafe {
        x86_64::instructions::interrupts::without_interrupts(|| {
            SCHEDULER.schedule();
        });
    }
}

// ---------------------------------------------------------------------------
// Built-in tasks
// ---------------------------------------------------------------------------

/// The idle task: runs when no other task is ready.
///
/// Burns CPU with `hlt` (pauses until the next interrupt, then the scheduler
/// will check if a real task became ready).
fn idle_task() -> ! {
    loop {
        // Safety: hlt is safe in ring 0; no memory side effects.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
