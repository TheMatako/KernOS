// kernel/src/drivers/e1000.rs
//
// Intel e1000 (82540EM) driver — PCI NIC.
//
// ── How it works ─────────────────────────────────────────────────────────────
//
// The e1000 is the default virtual NIC in QEMU (`-netdev user,model=e1000`).
// It exposes its control registers via an MMIO BAR (Base Address Register 0)
// mapped in PCI configuration space.
//
// CPU ↔ NIC communication uses two DMA descriptor rings in RAM:
//
//   TX ring : the driver writes descriptors (physical address of the packet +
//             length), then advances the "tail" pointer to notify the NIC that
//             packets are ready. The NIC reads the descriptors, transmits the
//             data on the wire, and marks the descriptors as "done" (DD bit).
//
//   RX ring : the driver allocates RAM buffers and writes their addresses into
//             the descriptors. The NIC writes received packets into these
//             buffers and advances its own "head" pointer. The driver reads
//             up to the "head" to retrieve incoming packets.
//
// ── Key Registers ────────────────────────────────────────────────────────────
//
//   CTRL    0x0000  — Device Control (reset, link up, ...)
//   STATUS  0x0008  — Device Status (link state)
//   EERD    0x0014  — EEPROM Read (used to extract MAC address)
//   ICR     0x00C0  — Interrupt Cause Read (ack IRQs)
//   IMS     0x00D0  — Interrupt Mask Set (enable IRQs)
//   RCTL    0x0100  — Receive Control
//   TCTL    0x0400  — Transmit Control
//   RDBAL   0x2800  — RX Descriptor Base Address Low
//   RDBAH   0x2804  — RX Descriptor Base Address High
//   RDLEN   0x2808  — RX Ring Length in bytes
//   RDH     0x2810  — RX Head (NIC writes here)
//   RDT     0x2818  — RX Tail (Driver writes here)
//   TDBAL   0x3800  — TX Descriptor Base Address Low
//   TDBAH   0x3804  — TX Descriptor Base Address High
//   TDLEN   0x3808  — TX Ring Length in bytes
//   TDH     0x3810  — TX Head (NIC writes here)
//   TDT     0x3818  — TX Tail (Driver writes here)
//   RAL0    0xE400  — Receive Address Low  (MAC dest filter 0)
//   RAH0    0xE404  — Receive Address High + valid bit

#![allow(dead_code)]
#![allow(static_mut_refs)]

use crate::drivers::pci;
use core::ptr;
// use crate::vmm::phys_to_virt; // Left for future MMU compatibility
// use crate::pmm;               // Left for future dynamic allocation
// use x86_64::PhysAddr;

// ---------------------------------------------------------------------------
// Register Constants
// ---------------------------------------------------------------------------

const REG_CTRL: u32 = 0x0000;
const REG_STATUS: u32 = 0x0008;
const REG_EERD: u32 = 0x0014;
const REG_ICR: u32 = 0x00C0;
const REG_IMS: u32 = 0x00D0;
const REG_RCTL: u32 = 0x0100;
const REG_TCTL: u32 = 0x0400;
const REG_RDBAL: u32 = 0x2800;
const REG_RDBAH: u32 = 0x2804;
const REG_RDLEN: u32 = 0x2808;
const REG_RDH: u32 = 0x2810;
const REG_RDT: u32 = 0x2818;
const REG_TDBAL: u32 = 0x3800;
const REG_TDBAH: u32 = 0x3804;
const REG_TDLEN: u32 = 0x3808;
const REG_TDH: u32 = 0x3810;
const REG_TDT: u32 = 0x3818;
const REG_RAL0: u32 = 0xE400;
const REG_RAH0: u32 = 0xE404;

// CTRL Bits
const CTRL_SLU: u32 = 1 << 6; // Set Link Up
const CTRL_RST: u32 = 1 << 26; // Device Reset

// RCTL Bits
const RCTL_EN: u32 = 1 << 1; // Receiver Enable
const RCTL_BAM: u32 = 1 << 15; // Broadcast Accept Mode
const RCTL_BSIZE: u32 = 0 << 16; // Buffer size = 2048 (bits[17:16]=00)
const RCTL_SECRC: u32 = 1 << 26; // Strip Ethernet CRC

// TCTL Bits
const TCTL_EN: u32 = 1 << 1; // Transmit Enable
const TCTL_PSP: u32 = 1 << 3; // Pad Short Packets
const TCTL_CT: u32 = 0x10 << 4; // Collision Threshold
const TCTL_COLD: u32 = 0x40 << 12; // Collision Distance

// TX Descriptor Bits
const TX_CMD_EOP: u8 = 1 << 0; // End Of Packet
const TX_CMD_IFCS: u8 = 1 << 1; // Insert FCS (CRC)
const TX_CMD_RS: u8 = 1 << 3; // Report Status
const TX_STA_DD: u8 = 1 << 0; // Descriptor Done

// RX Descriptor Bits
const RX_STA_DD: u8 = 1 << 0; // Descriptor Done
const RX_STA_EOP: u8 = 1 << 1; // End Of Packet

// Intel 82540EM PCI IDs (QEMU e1000)
const E1000_VENDOR: u16 = 0x8086;
const E1000_DEVICE: u16 = 0x100E;

// ---------------------------------------------------------------------------
// Ring Geometry
// ---------------------------------------------------------------------------

/// Number of TX/RX descriptors. Must be a multiple of 8.
const RING_SIZE: usize = 32;

/// Size of a single RX packet buffer (2 KiB — accommodates max Ethernet frame).
const RX_BUF_SIZE: usize = 2048;

// ---------------------------------------------------------------------------
// DMA Descriptors
// ---------------------------------------------------------------------------

/// TX Descriptor (16 bytes, hardware-defined layout).
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct TxDesc {
    /// Physical address of the data buffer.
    addr: u64,
    /// Packet length in bytes.
    length: u16,
    /// Checksum offset (unused here).
    cso: u8,
    /// Commands (EOP | IFCS | RS).
    cmd: u8,
    /// Status (DD = done).
    status: u8,
    /// Checksum start (unused).
    css: u8,
    /// VLAN tag (unused).
    special: u16,
}

/// RX Descriptor (16 bytes).
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct RxDesc {
    /// Physical address of the receive buffer.
    addr: u64,
    /// Received packet length (filled by NIC).
    length: u16,
    /// Checksum (ignored).
    checksum: u16,
    /// Status (DD | EOP).
    status: u8,
    /// Errors (ignored).
    errors: u8,
    /// VLAN tag (ignored).
    special: u16,
}

// ---------------------------------------------------------------------------
// DMA Rings — stored in .bss, 16-byte aligned
// ---------------------------------------------------------------------------

#[repr(align(16))]
struct TxRing([TxDesc; RING_SIZE]);

#[repr(align(16))]
struct RxRing([RxDesc; RING_SIZE]);

static mut TX_RING: TxRing = TxRing(
    [TxDesc {
        addr: 0,
        length: 0,
        cso: 0,
        cmd: 0,
        status: 0,
        css: 0,
        special: 0,
    }; RING_SIZE],
);

static mut RX_RING: RxRing = RxRing(
    [RxDesc {
        addr: 0,
        length: 0,
        checksum: 0,
        status: 0,
        errors: 0,
        special: 0,
    }; RING_SIZE],
);

/// RX Buffers — 32 × 2 KiB = 64 KiB in .bss.
#[repr(align(16))]
struct RxBuffers([[u8; RX_BUF_SIZE]; RING_SIZE]);
static mut RX_BUFS: RxBuffers = RxBuffers([[0u8; RX_BUF_SIZE]; RING_SIZE]);

/// TX Buffers — 32 × 2 KiB = 64 KiB in .bss.
#[repr(align(16))]
struct TxBuffers([[u8; RX_BUF_SIZE]; RING_SIZE]);
static mut TX_BUFS: TxBuffers = TxBuffers([[0u8; RX_BUF_SIZE]; RING_SIZE]);

// ---------------------------------------------------------------------------
// Driver State
// ---------------------------------------------------------------------------

struct E1000State {
    /// Virtual address of the NIC's MMIO BAR.
    mmio_base: u64,
    /// Hardware MAC address of this NIC (6 bytes).
    mac: [u8; 6],
    /// Next TX descriptor to write to.
    tx_tail: usize,
    /// Next RX descriptor to read from.
    rx_head: usize,
    /// Initialization flag.
    ready: bool,
}

static mut E1000: E1000State = E1000State {
    mmio_base: 0,
    mac: [0u8; 6],
    tx_tail: 0,
    rx_head: 0,
    ready: false,
};

// ---------------------------------------------------------------------------
// MMIO Register Access
// ---------------------------------------------------------------------------

/// Reads a 32-bit register from the NIC.
///
/// # Safety
/// `mmio_base` must be mapped (identity mapped first 4 GiB, including the PCI BAR).
#[inline]
unsafe fn reg_read(mmio_base: u64, reg: u32) -> u32 {
    ptr::read_volatile((mmio_base + reg as u64) as *const u32)
}

/// Writes a 32-bit register to the NIC.
///
/// # Safety
/// Same constraints as `reg_read`.
#[inline]
unsafe fn reg_write(mmio_base: u64, reg: u32, val: u32) {
    ptr::write_volatile((mmio_base + reg as u64) as *mut u32, val);
}

// ---------------------------------------------------------------------------
// MAC Address Extraction via EEPROM
// ---------------------------------------------------------------------------

/// Reads the MAC address from the EEPROM via the EERD register.
///
/// The e1000 EEPROM stores the MAC in words 0, 1, and 2.
/// One EEPROM "word" = 2 bytes.
///
/// # Safety
/// MMIO access.
unsafe fn read_mac(mmio_base: u64) -> [u8; 6] {
    let mut mac = [0u8; 6];
    for word in 0..3usize {
        // Initiate EEPROM read: write (addr << 8) | START into EERD.
        reg_write(mmio_base, REG_EERD, ((word as u32) << 8) | 1);
        // Wait for the DONE bit (bit 4) to be set by the NIC.
        let mut val;
        loop {
            val = reg_read(mmio_base, REG_EERD);
            if val & (1 << 4) != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        // The 16-bit data payload is in val[31:16].
        let data = (val >> 16) as u16;
        mac[word * 2] = (data & 0xFF) as u8;
        mac[word * 2 + 1] = (data >> 8) as u8;
    }
    mac
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initializes the e1000 driver.
///
/// Uses `pci::find_device` to locate the NIC, extracts the MMIO address from
/// BAR 0, configures the TX/RX DMA rings, and retrieves the MAC address.
///
/// # Safety
/// - PCI must be initialized (`pci::init()` called previously).
/// - VMM direct map must be active (`vmm::init()` called previously).
/// - Mutates `static mut` state.
pub unsafe fn init() {
    // ── 1. Locate the e1000 in the PCI device table ──────────────────────────
    let dev = match pci::find_device(E1000_VENDOR, E1000_DEVICE) {
        Some(d) => d,
        None => {
            crate::kprintln!(
                "[E1000] NIC not found (vendor={:#x} device={:#x})",
                E1000_VENDOR,
                E1000_DEVICE
            );
            return;
        }
    };

    // ── 2. Extract BAR 0 (MMIO Base Address) ─────────────────────────────────
    // BAR bits [3:0] encode the type (memory/IO, 32/64-bit).
    // For a 32-bit memory BAR: bits [31:4] = physical address.
    let bar0_raw = dev.bars[0];
    let mmio_phys = (bar0_raw & 0xFFFF_FFF0) as u64;

    // The BAR is typically in the first 4 GiB -> covered by the VMM identity map.
    let mmio_virt = mmio_phys;

    crate::kprintln!("[E1000] NIC discovered: BAR0 MMIO phys={:#x}", mmio_phys);

    // ── 3. Enable Bus Mastering (Crucial for DMA) ────────────────────────────
    pci::enable_bus_mastering(dev);

    // ── 4. Hardware Reset ────────────────────────────────────────────────────
    reg_write(
        mmio_virt,
        REG_CTRL,
        reg_read(mmio_virt, REG_CTRL) | CTRL_RST,
    );
    // Wait for reset to complete (RST bit clears itself).
    loop {
        if reg_read(mmio_virt, REG_CTRL) & CTRL_RST == 0 {
            break;
        }
        core::hint::spin_loop();
    }

    // ── 5. Read MAC Address ──────────────────────────────────────────────────
    let mac = read_mac(mmio_virt);
    E1000.mac = mac;
    crate::kprintln!(
        "[E1000] MAC Address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    // ── 6. Configure Receive Address Filters (RAL/RAH) ───────────────────────
    let ral = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
    let rah = u32::from_le_bytes([mac[4], mac[5], 0, 0]) | (1 << 31); // AV bit
    reg_write(mmio_virt, REG_RAL0, ral);
    reg_write(mmio_virt, REG_RAH0, rah);

    // ── 7. Configure the TX Ring ─────────────────────────────────────────────
    // Physical address of TX_RING (static -> identity-mapped).
    let tx_ring_phys = ptr::addr_of!(TX_RING) as u64;

    // Pre-fill the TX descriptors with their corresponding buffer addresses.
    for i in 0..RING_SIZE {
        TX_RING.0[i].addr = ptr::addr_of!(TX_BUFS.0[i]) as u64;
        TX_RING.0[i].status = TX_STA_DD; // Mark "done" so send() can use them
    }

    reg_write(mmio_virt, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
    reg_write(mmio_virt, REG_TDBAH, (tx_ring_phys >> 32) as u32);
    reg_write(
        mmio_virt,
        REG_TDLEN,
        (RING_SIZE * core::mem::size_of::<TxDesc>()) as u32,
    );
    reg_write(mmio_virt, REG_TDH, 0);
    reg_write(mmio_virt, REG_TDT, 0);

    // Enable TX: EN | PSP | CT | COLD
    reg_write(
        mmio_virt,
        REG_TCTL,
        TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD,
    );

    // ── 8. Configure the RX Ring ─────────────────────────────────────────────
    let rx_ring_phys = ptr::addr_of!(RX_RING) as u64;
    for i in 0..RING_SIZE {
        RX_RING.0[i].addr = ptr::addr_of!(RX_BUFS.0[i]) as u64;
        RX_RING.0[i].status = 0; // Not yet received
    }

    reg_write(mmio_virt, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
    reg_write(mmio_virt, REG_RDBAH, (rx_ring_phys >> 32) as u32);
    reg_write(
        mmio_virt,
        REG_RDLEN,
        (RING_SIZE * core::mem::size_of::<RxDesc>()) as u32,
    );
    reg_write(mmio_virt, REG_RDH, 0);
    reg_write(mmio_virt, REG_RDT, (RING_SIZE - 1) as u32); // Hand all buffers to the NIC

    // Enable RX: EN | BAM | BSIZE | SECRC
    reg_write(
        mmio_virt,
        REG_RCTL,
        RCTL_EN | RCTL_BAM | RCTL_BSIZE | RCTL_SECRC,
    );

    // ── 9. Set Link Up ───────────────────────────────────────────────────────
    reg_write(
        mmio_virt,
        REG_CTRL,
        reg_read(mmio_virt, REG_CTRL) | CTRL_SLU,
    );

    E1000.mmio_base = mmio_virt;
    E1000.tx_tail = 0;
    E1000.rx_head = 0;
    E1000.ready = true;

    crate::kprintln!(
        "[E1000] init OK — TX/RX rings ({} descs each), Link UP",
        RING_SIZE
    );
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns the MAC address of this NIC.
pub fn mac_address() -> [u8; 6] {
    unsafe { E1000.mac }
}

/// Sends a raw Ethernet frame (without CRC — the NIC appends it).
///
/// `data` must contain the exact Ethernet frame: dst_mac(6) + src_mac(6)
/// + ethertype(2) + payload.
///
/// Returns `Err` if the NIC is not ready or if the TX ring is full.
///
/// # Safety
/// MMIO access and writes to static TX buffers.
pub unsafe fn send(data: &[u8]) -> Result<(), &'static str> {
    if !E1000.ready {
        return Err("e1000: not ready");
    }
    if data.len() > RX_BUF_SIZE {
        return Err("e1000: packet too large");
    }

    let tail = E1000.tx_tail;
    let desc = &mut TX_RING.0[tail];

    // Spin-wait until the descriptor is freed by the NIC (DD bit set).
    let deadline = 100_000u32;
    let mut wait = 0u32;
    while unsafe { core::ptr::read_volatile(&desc.status) } & TX_STA_DD == 0 {
        wait += 1;
        if wait >= deadline {
            return Err("e1000: TX ring full");
        }
        core::hint::spin_loop();
    }

    // Copy the packet into the TX buffer.
    let buf = &mut TX_BUFS.0[tail];
    buf[..data.len()].copy_from_slice(data);

    // Populate the descriptor.
    desc.addr = buf.as_ptr() as u64;
    desc.length = data.len() as u16;
    desc.cmd = TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS;
    desc.status = 0; // Clear DD so the NIC knows there's work to do

    // Advance the tail to notify the NIC.
    let new_tail = (tail + 1) % RING_SIZE;
    E1000.tx_tail = new_tail;
    reg_write(E1000.mmio_base, REG_TDT, new_tail as u32);

    Ok(())
}

/// Receives the next available packet from the RX ring.
///
/// Copies the data into `out` and returns its length, or `Ok(0)` if
/// no packet is currently available.
///
/// # Safety
/// MMIO access and reads from static RX buffers.
pub unsafe fn recv(out: &mut [u8]) -> Result<usize, &'static str> {
    if !E1000.ready {
        return Err("e1000: not ready");
    }

    let head = E1000.rx_head;
    let desc = &mut RX_RING.0[head];

    // DD bit is set by the NIC when it has successfully written a packet.
    if desc.status & RX_STA_DD == 0 {
        return Ok(0); // No packet available.
    }

    let len = desc.length as usize;
    if len > out.len() {
        return Err("e1000: recv buffer too small");
    }

    // Copy from the RX buffer into the caller's buffer.
    out[..len].copy_from_slice(&RX_BUFS.0[head][..len]);

    // Reset the descriptor and yield it back to the NIC.
    desc.status = 0;
    desc.addr = ptr::addr_of!(RX_BUFS.0[head]) as u64;

    // Advance the RX tail to hand the buffer ownership back to the hardware.
    reg_write(E1000.mmio_base, REG_RDT, head as u32);

    // Advance our software read cursor.
    E1000.rx_head = (head + 1) % RING_SIZE;

    Ok(len)
}

/// Returns `true` if the NIC is initialized and the link is UP.
pub fn is_ready() -> bool {
    unsafe { E1000.ready }
}
