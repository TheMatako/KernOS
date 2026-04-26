// kernel/src/gdt.rs
//
// Global Descriptor Table (GDT) and Task State Segment (TSS).
//
// ── Background ────────────────────────────────────────────────────────────────
//
// The GDT is a table of 8-byte "segment descriptors" that the CPU consults
// every time code runs or memory is accessed.  In 64-bit long mode, base and
// limit fields are mostly ignored (flat address space), but the GDT is still
// mandatory for two reasons:
//
//   1. The *type* and *privilege level* bits in CS (Code Segment) tell the CPU
//      whether we are in ring 0 (kernel) or ring 3 (user).
//
//   2. The GDT must contain a pointer to the TSS, which the CPU reads to find
//      the stack to switch to when handling certain exceptions.
//
// ── Our GDT layout ────────────────────────────────────────────────────────────
//
//   Index 0 : Null descriptor   — mandatory; CPU faults if CS/SS = 0.
//   Index 1 : Kernel code (64-bit, ring 0, executable).
//   Index 2 : Kernel data (ring 0, read/write).
//   Index 3 : TSS low  ─┐ A 64-bit TSS descriptor is 16 bytes wide,
//   Index 4 : TSS high ─┘ so it occupies *two* GDT slots.
//
// ── TSS and IST ───────────────────────────────────────────────────────────────
//
// The TSS (Task State Segment) is a CPU structure that holds, among other
// things, the Interrupt Stack Table (IST): up to 7 alternate stacks the CPU
// can switch to before dispatching a specific exception.
//
// We give IST entry 0 to the double fault (#DF) handler.  Without a dedicated
// stack, a #DF caused by a kernel stack overflow would immediately triple-fault
// (the CPU would try to push the exception frame onto the exhausted stack,
// fault again, and then give up and reset).
//
// The `syscall`/`sysret` instructions derive segment selectors from the STAR
// MSR using fixed arithmetic:
//
//   SYSCALL  → CS = STAR[47:32],     SS = STAR[47:32] + 8
//   SYSRET64 → SS = STAR[63:48] + 8, CS = STAR[63:48] + 16
//
// To satisfy both equations simultaneously we need:
//
//   kernel code  = 0x08   STAR[47:32]      = 0x08  → SS = 0x10 (kernel data ✓)
//   kernel data  = 0x10
//   user data    = 0x1B   STAR[63:48]      = 0x13  → SS = 0x1B (user data  ✓)
//   user code    = 0x23                            → CS = 0x23 (user code  ✓)
//   TSS low      = 0x28
//   TSS high     = 0x30  (16-byte TSS descriptor occupies two slots)
//
// Index | Offset | Descriptor      | Selector (RPL)
// ──────┼────────┼─────────────────┼────────────────
//   0   |  0x00  | Null            | —
//   1   |  0x08  | Kernel code 64  | 0x08  (RPL=0)
//   2   |  0x10  | Kernel data     | 0x10  (RPL=0)
//   3   |  0x18  | User data       | 0x1B  (RPL=3)
//   4   |  0x20  | User code 64    | 0x23  (RPL=3)
//   5   |  0x28  | TSS low         | 0x28
//   6   |  0x30  | TSS high        | —

#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::mem::MaybeUninit;
use core::ptr::addr_of;
use x86_64::{
    instructions::tables::load_tss,
    registers::segmentation::{Segment, CS, DS, ES, SS},
    structures::{
        gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector},
        tss::TaskStateSegment,
    },
    VirtAddr,
};

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Public constants — consumed by syscall.rs (STAR MSR)
// ---------------------------------------------------------------------------

/// IST index used by the #DF handler (0-based in the x86_64 crate array).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Kernel code segment selector (RPL=0).  STAR[47:32].
pub const KCODE_SELECTOR: u16 = 0x08;

/// Kernel data segment selector (RPL=0).  SS on syscall entry.
pub const KDATA_SELECTOR: u16 = 0x10;

/// User data segment selector (RPL=3).
/// SYSRET sets SS = STAR[63:48] + 8 = 0x13 + 8 = 0x1B.
pub const UDATA_SELECTOR: u16 = 0x1B;

/// User code segment selector (RPL=3).
/// SYSRET sets CS = STAR[63:48] + 16 = 0x13 + 16 = 0x23.
pub const UCODE_SELECTOR: u16 = 0x23;

/// Value written into STAR[63:48] to produce the right user selectors on SYSRET.
/// 0x1B - 8 = 0x13.
pub const STAR_USER_BASE: u16 = 0x13;

// ---------------------------------------------------------------------------
// Private constants
// ---------------------------------------------------------------------------

/// Size of the dedicated double-fault stack: 5 × 4 KiB = 20 KiB.
///
/// This must be large enough for the panic!() call chain triggered by a #DF.
const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 5;

// ---------------------------------------------------------------------------
// Static storage
// ---------------------------------------------------------------------------

/// Backing storage for the double-fault handler stack.
///
/// `#[repr(align(16))]` ensures 16-byte stack alignment as required by the
/// System V AMD64 ABI (the CPU also mandates 16-byte alignment for RSP on
/// exception entry when using an IST entry).
#[repr(align(16))]
struct AlignedStack([u8; DOUBLE_FAULT_STACK_SIZE]);

/// The physical stack bytes used by the #DF exception handler.
///
/// Lives in `.bss` (zeroed by `zero_bss()` in main.rs before we get here).
/// Its address is written into `TSS.interrupt_stack_table[0]`.
static mut DOUBLE_FAULT_STACK: AlignedStack = AlignedStack([0; DOUBLE_FAULT_STACK_SIZE]);

/// The Task State Segment.
///
/// `TaskStateSegment::new()` is a `const fn` that zero-initialises the struct.
/// We populate `interrupt_stack_table[0]` in `init()`.
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// The Global Descriptor Table.
///
/// `GlobalDescriptorTable::new()` creates a table containing only the
/// mandatory null descriptor.  We call `append()` in `init()` to append
/// the code, data, and TSS descriptors.
///
/// The GDT must be a `static` because `lgdt` only stores a *pointer* to it;
/// the CPU will keep reading from this address on every context switch.
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();

/// The segment selectors assigned to our GDT entries.
///
/// Populated by `init()`.  Other modules (e.g. idt.rs) call
/// `gdt::selectors()` to retrieve them.
static mut SELECTORS: MaybeUninit<KernelSelectors> = MaybeUninit::uninit();

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Segment selectors pointing into our GDT.
///
/// A `SegmentSelector` is a 16-bit value: the top 13 bits are the GDT index,
/// bit 2 is the Table Indicator (0 = GDT), bits 1:0 are the RPL (Requested
/// Privilege Level).
/// All segment selectors produced by `init()`.
#[derive(Debug, Clone, Copy)]
pub struct KernelSelectors {
    pub kcode: SegmentSelector,
    pub kdata: SegmentSelector,
    pub udata: SegmentSelector,
    pub ucode: SegmentSelector,
    pub tss: SegmentSelector,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns the kernel segment selectors.
///
/// # Panics
/// Panics if `gdt::init()` has not been called yet.
pub fn selectors() -> KernelSelectors {
    // Safety: `init()` has been called and `SELECTORS` is fully written.
    unsafe { SELECTORS.assume_init() }
}

/// Initialises and loads the GDT, TSS, and kernel segment registers.
///
/// Call order in `kernel_main`:
///   1. `zero_bss()`    (mandatory — static mut globals live in .bss)
///   2. `serial::init()`
///   3. **`gdt::init()`**
///   4. `idt::init()`
///
/// # Safety (internal)
/// This function writes to `static mut` globals and executes privileged
/// instructions (`lgdt`, `ltr`, segment-register loads).
/// Must be called exactly once, on the BSP, before interrupts are enabled.
pub fn init() {
    unsafe {
        // ── Step 1 — Set the IST[0] pointer in the TSS ───────────────────────
        //
        // x86 stacks grow *downward*, so the CPU needs the *top* (highest
        // address) of the stack region.  We compute it as:
        //   base address of DOUBLE_FAULT_STACK  +  size of the array
        //
        // `addr_of!` gives us a raw pointer without creating a Rust reference,
        // which avoids the undefined-behaviour of `&raw` on a `static mut`.
        let stack_base = addr_of!(DOUBLE_FAULT_STACK) as u64;
        let stack_top = stack_base + DOUBLE_FAULT_STACK_SIZE as u64;

        // IST indices in the TSS are 1-based in the hardware spec but the
        // x86_64 crate uses a 0-based array; index 0 → IST1 in hardware.
        TSS.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = VirtAddr::new(stack_top);
        TSS.privilege_stack_table[0] = VirtAddr::new(stack_top);
        // ── Step 2 — Add descriptors to the GDT ──────────────────────────────
        //
        // `append` appends the descriptor and returns the corresponding
        // SegmentSelector.  The null descriptor at index 0 is already present.
        let kcode = GDT.append(Descriptor::kernel_code_segment()); // 0x08
        let kdata = GDT.append(Descriptor::kernel_data_segment()); // 0x10
        let udata = GDT.append(Descriptor::user_data_segment()); // 0x1B (RPL=3 set by crate)
        let ucode = GDT.append(Descriptor::user_code_segment()); // 0x23 (RPL=3)
        let tss = GDT.append(Descriptor::tss_segment(&TSS)); // 0x28+0x30

        // Persist the selectors for later use.
        SELECTORS.write(KernelSelectors {
            kcode,
            kdata,
            udata,
            ucode,
            tss,
        });

        // ── Step 3 — Load the GDT (`lgdt`) ───────────────────────────────────
        //
        // After this, the CPU's GDTR register points to our `GDT` static.
        // The segment registers still hold the old UEFI selectors; we fix
        // that in step 4.
        GDT.load();

        // ── Step 4 — Reload segment registers ────────────────────────────────
        //
        // CS cannot be set with a simple `mov`; a far jump or far return is
        // required.  The x86_64 crate handles this correctly with a `retfq`.
        CS::set_reg(kcode);

        // DS, ES, SS accept a plain `mov reg, sel`.
        // FS and GS are reserved for thread-local storage / per-CPU data
        // (future bricks); we leave them as 0 (null selector) for now.
        DS::set_reg(kdata);
        ES::set_reg(kdata);
        SS::set_reg(kdata);

        // ── Step 5 — Load the TSS (`ltr`) ────────────────────────────────────
        //
        // `ltr` (Load Task Register) writes the TSS selector into the hidden
        // Task Register.  The CPU then knows where to find our IST stacks when
        // dispatching exceptions.  Must be called *after* `lgdt` and *after*
        // the GDT entry for the TSS has been populated.
        load_tss(tss);
    }

    crate::kprintln!(
        "[GDT] loaded — kcode={:#x} kdata={:#x} ucode={:#x} udata={:#x}",
        KCODE_SELECTOR,
        KDATA_SELECTOR,
        UCODE_SELECTOR,
        UDATA_SELECTOR,
    );
}
