// kernel/src/drivers/pci.rs
//
// PCI bus configuration space enumerator.
//
// ── Background ────────────────────────────────────────────────────────────────
//
// PCI (Peripheral Component Interconnect) is the standard bus for hardware
// devices on a PC: network cards, storage controllers, GPUs, USB controllers…
//
// Every PCI device is uniquely identified by three numbers:
//   Bus  (0–255) : which PCI bus it sits on.
//   Slot (0–31)  : which slot on that bus (also called "device").
//   Func (0–7)   : which function within the device (multi-function devices).
//
// Each device exposes a 256-byte "configuration space" readable through two
// legacy I/O ports (the "CAM" — Configuration Access Mechanism):
//
//   0xCF8  CONFIG_ADDRESS — write a 32-bit address here …
//   0xCFC  CONFIG_DATA    — … then read/write 32 bits of config space here.
//
// The address format for CONFIG_ADDRESS:
//   bit 31       : Enable bit (must be 1)
//   bits [30:24] : reserved (0)
//   bits [23:16] : bus number
//   bits [15:11] : slot (device) number
//   bits [10:8]  : function number
//   bits [7:2]   : register offset (DWord index)
//   bits [1:0]   : 0 (must be zero — reads are always DWord-aligned)
//
// ── Device discovery ─────────────────────────────────────────────────────────
//
// We perform a "brute-force" scan: try every (bus, slot, function) triple and
// check whether Vendor ID != 0xFFFF (0xFFFF means "no device present").
//
// We only scan bus 0 here (the root bus), but follow PCI-to-PCI bridge
// secondary bus numbers to enumerate the full hierarchy.
//
// ── Device table ─────────────────────────────────────────────────────────────
//
// Found devices are stored in a static array of `PciDevice` structs.
// Brick 9 (TCP/IP) will call `pci::find_device(vendor, device)` to locate
// the NIC and obtain its BAR addresses.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use x86_64::instructions::port::Port;

// ---------------------------------------------------------------------------
// I/O ports
// ---------------------------------------------------------------------------

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

// ---------------------------------------------------------------------------
// Configuration space register offsets (DWord indices × 4 = byte offset)
// ---------------------------------------------------------------------------

const CFG_VENDOR_DEVICE: u8 = 0x00; // [15:0]=VendorID  [31:16]=DeviceID
const CFG_STATUS_COMMAND: u8 = 0x04; // [15:0]=Command   [31:16]=Status
const CFG_CLASS_REV: u8 = 0x08; // [7:0]=Revision   [31:8]=ClassCode
const CFG_BIST_HEADER: u8 = 0x0C; // [23:16]=HeaderType
const CFG_BAR0: u8 = 0x10;
const CFG_BAR1: u8 = 0x14;
const CFG_BAR2: u8 = 0x18;
const CFG_BAR3: u8 = 0x1C;
const CFG_BAR4: u8 = 0x20;
const CFG_BAR5: u8 = 0x24;
const CFG_SUBSYS: u8 = 0x2C; // [15:0]=SubVendor [31:16]=SubDevice
const CFG_IRQ: u8 = 0x3C; // [7:0]=IRQ line   [15:8]=IRQ pin

/// Header type mask: bits [6:0] of byte 0x0E.
/// Type 0 = endpoint device.
/// Type 1 = PCI-to-PCI bridge.
const HEADER_TYPE_MASK: u8 = 0x7F;
const HEADER_TYPE_ENDPOINT: u8 = 0x00;
const HEADER_TYPE_PCI_BRIDGE: u8 = 0x01;
/// Bit 7 of the header type byte: set if the device is multi-function.
const HEADER_MULTIFUNCTION: u8 = 0x80;

// PCI class codes (byte [31:24] of register 0x08).
pub const CLASS_UNCLASSIFIED: u8 = 0x00;
pub const CLASS_MASS_STORAGE: u8 = 0x01;
pub const CLASS_NETWORK: u8 = 0x02;
pub const CLASS_DISPLAY: u8 = 0x03;
pub const CLASS_MULTIMEDIA: u8 = 0x04;
pub const CLASS_BRIDGE: u8 = 0x06;
pub const CLASS_SERIAL_BUS: u8 = 0x0C;

// ---------------------------------------------------------------------------
// PciDevice
// ---------------------------------------------------------------------------

/// Key information extracted from a PCI device's configuration space.
///
/// Stored in the global device table after enumeration.
/// Brick 9 will add fields for the NIC's MMIO BAR, DMA ring base, etc.
#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8, // base class
    pub subclass: u8,
    pub prog_if: u8, // programming interface
    pub revision: u8,
    /// Base Address Registers 0–5.
    /// For memory BARs: bits [31:4] = base address (mask out bits [3:0]).
    /// For I/O BARs:    bits [31:2] = base address (mask out bits [1:0] + bit 0 = 1).
    pub bars: [u32; 6],
    /// IRQ line as reported by the firmware (may be 0xFF = not connected).
    pub irq_line: u8,
}

impl PciDevice {
    const fn zero() -> Self {
        Self {
            bus: 0,
            slot: 0,
            func: 0,
            vendor_id: 0,
            device_id: 0,
            class: 0,
            subclass: 0,
            prog_if: 0,
            revision: 0,
            bars: [0u32; 6],
            irq_line: 0xFF,
        }
    }

    /// Returns `true` if this device's class matches the given PCI class code.
    pub fn is_class(&self, class: u8) -> bool {
        self.class == class
    }

    /// Returns a human-readable string for the base class code.
    pub fn class_name(&self) -> &'static str {
        match self.class {
            CLASS_MASS_STORAGE => "Mass Storage",
            CLASS_NETWORK => "Network",
            CLASS_DISPLAY => "Display",
            CLASS_MULTIMEDIA => "Multimedia",
            CLASS_BRIDGE => "Bridge",
            CLASS_SERIAL_BUS => "Serial Bus",
            _ => "Unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Static device table
// ---------------------------------------------------------------------------

/// Maximum number of PCI devices we track.
///
/// A typical QEMU VM has 4–8 devices; a real server may have 32+.
const MAX_PCI_DEVICES: usize = 64;

/// The discovered PCI device list.
static mut DEVICES: [PciDevice; MAX_PCI_DEVICES] = [PciDevice::zero(); MAX_PCI_DEVICES];
/// Number of valid entries in `DEVICES`.
static mut DEVICE_COUNT: usize = 0;

// ---------------------------------------------------------------------------
// Low-level config space access
// ---------------------------------------------------------------------------

/// Builds the 32-bit value written to CONFIG_ADDRESS.
///
/// Encodes bus, slot, function, and register offset into the hardware format.
#[inline]
fn make_address(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    // Enable bit (31) | bus (23:16) | slot (15:11) | func (10:8) | reg (7:2)
    (1u32 << 31)
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((offset & 0xFC) as u32) // mask bits [1:0] to zero (DWord aligned)
}

/// Reads a 32-bit DWord from PCI configuration space.
///
/// # Safety
/// Writes to CONFIG_ADDRESS (0xCF8), reads from CONFIG_DATA (0xCFC).
/// Must be called in ring 0 with exclusive I/O port access.
pub unsafe fn config_read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
    let mut data_port: Port<u32> = Port::new(CONFIG_DATA);

    addr_port.write(make_address(bus, slot, func, offset));
    data_port.read()
}

/// Writes a 32-bit DWord to PCI configuration space.
///
/// Used to enable bus mastering, I/O space, or MMIO on a device.
///
/// # Safety
/// Same constraints as `config_read32`.
pub unsafe fn config_write32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
    let mut data_port: Port<u32> = Port::new(CONFIG_DATA);

    addr_port.write(make_address(bus, slot, func, offset));
    data_port.write(value);
}

/// Reads the 16-bit Vendor ID of a (bus, slot, function).
///
/// Returns `0xFFFF` if no device is present.
///
/// # Safety
/// Same as `config_read32`.
pub unsafe fn read_vendor_id(bus: u8, slot: u8, func: u8) -> u16 {
    (config_read32(bus, slot, func, CFG_VENDOR_DEVICE) & 0xFFFF) as u16
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// Reads all interesting fields from a device's config space and appends a
/// `PciDevice` entry to the global table.
///
/// # Safety
/// Reads PCI config space. Writes to `static mut DEVICES`.
unsafe fn register_device(bus: u8, slot: u8, func: u8) {
    if unsafe { DEVICE_COUNT } >= MAX_PCI_DEVICES {
        crate::kprintln!(
            "[PCI]  WARNING: device table full, skipping {:02x}:{:02x}.{}",
            bus,
            slot,
            func
        );
        return;
    }

    let vd = config_read32(bus, slot, func, CFG_VENDOR_DEVICE);
    let cr = config_read32(bus, slot, func, CFG_CLASS_REV);
    let irq_dw = config_read32(bus, slot, func, CFG_IRQ);

    let mut dev = PciDevice {
        bus,
        slot,
        func,
        vendor_id: (vd & 0xFFFF) as u16,
        device_id: (vd >> 16) as u16,
        class: (cr >> 24) as u8,
        subclass: (cr >> 16) as u8,
        prog_if: (cr >> 8) as u8,
        revision: (cr & 0xFF) as u8,
        bars: [0u32; 6],
        irq_line: (irq_dw & 0xFF) as u8,
    };

    // Read all 6 BARs.
    let bar_offsets = [CFG_BAR0, CFG_BAR1, CFG_BAR2, CFG_BAR3, CFG_BAR4, CFG_BAR5];
    for (i, &off) in bar_offsets.iter().enumerate() {
        dev.bars[i] = config_read32(bus, slot, func, off);
    }

    //crate::kprintln!(
    //    "[PCI]  {:02x}:{:02x}.{}  {:04x}:{:04x}  {}  IRQ={}",
    //    bus,
    //    slot,
    //    func,
    //    dev.vendor_id,
    //    dev.device_id,
    //    dev.class_name(),
    //    dev.irq_line,
    //);

    let idx = unsafe { DEVICE_COUNT };
    unsafe {
        DEVICES[idx] = dev;
        DEVICE_COUNT += 1;
    }
}

/// Scans one (bus, slot, function) and recurses into PCI-to-PCI bridges.
///
/// # Safety
/// Reads PCI config space. Must be called with interrupts disabled.
unsafe fn scan_function(bus: u8, slot: u8, func: u8) {
    let vendor = read_vendor_id(bus, slot, func);
    if vendor == 0xFFFF {
        return; // No device.
    }

    register_device(bus, slot, func);

    // If this is a PCI-to-PCI bridge (class=0x06, subclass=0x04), recurse
    // into the secondary bus it exposes.
    let cr = config_read32(bus, slot, func, CFG_CLASS_REV);
    let class = (cr >> 24) as u8;
    let subclass = (cr >> 16) as u8;

    if class == CLASS_BRIDGE && subclass == 0x04 {
        // Secondary bus number is in byte 0x19 of the bridge's config space.
        // DWord at 0x18 = [Primary Bus | Secondary Bus | Subordinate Bus | …]
        let bus_nums = config_read32(bus, slot, func, 0x18);
        let secondary = ((bus_nums >> 8) & 0xFF) as u8;

        // Scan the secondary bus.
        for new_slot in 0u8..32 {
            scan_slot(secondary, new_slot);
        }
    }
}

/// Scans all functions of a (bus, slot).
///
/// # Safety
/// Same as `scan_function`.
unsafe fn scan_slot(bus: u8, slot: u8) {
    let vendor = read_vendor_id(bus, slot, 0);
    if vendor == 0xFFFF {
        return; // No device in this slot.
    }

    // Scan function 0 (always present if vendor != 0xFFFF).
    scan_function(bus, slot, 0);

    // Check if the device is multi-function (header type bit 7).
    let hdr = config_read32(bus, slot, 0, CFG_BIST_HEADER);
    let header_type = ((hdr >> 16) & 0xFF) as u8;

    if header_type & HEADER_MULTIFUNCTION != 0 {
        // Scan functions 1–7.
        for func in 1u8..8 {
            if read_vendor_id(bus, slot, func) != 0xFFFF {
                scan_function(bus, slot, func);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enumerates the PCI bus and populates the device table.
///
/// Scans bus 0 and follows bridges to secondary buses.
///
/// # Safety
/// Reads I/O ports. Must be called once, with interrupts disabled.
pub unsafe fn init() {
    crate::kprintln!("[PCI]  scanning PCI bus...");
    for slot in 0u8..32 {
        scan_slot(0, slot);
    }
    crate::kprintln!("[PCI]  found {} device(s).", unsafe { DEVICE_COUNT });
}

/// Returns a slice of all discovered PCI devices.
pub fn devices() -> &'static [PciDevice] {
    // Safety: DEVICES is written only during init() and is read-only afterwards.
    unsafe { &DEVICES[..DEVICE_COUNT] }
}

/// Finds the first device matching (vendor_id, device_id).
///
/// Used by Brick 9 to locate the e1000 NIC:
///   `pci::find_device(0x8086, 0x100E)` — Intel e1000
pub fn find_device(vendor_id: u16, device_id: u16) -> Option<&'static PciDevice> {
    devices()
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.device_id == device_id)
}

/// Finds the first device matching a PCI class code.
///
/// Example: `pci::find_class(pci::CLASS_NETWORK)` → first NIC.
pub fn find_class(class: u8) -> Option<&'static PciDevice> {
    devices().iter().find(|d| d.class == class)
}

/// Enables Bus Mastering on a device (required for DMA).
///
/// Sets bit 2 of the PCI Command register.
/// Must be called before a DMA-capable driver starts any DMA transaction.
///
/// # Safety
/// Writes to PCI config space.
pub unsafe fn enable_bus_mastering(dev: &PciDevice) {
    let cmd_dw = config_read32(dev.bus, dev.slot, dev.func, CFG_STATUS_COMMAND);
    let cmd = (cmd_dw & 0xFFFF) as u16;

    // Bit 2 = Bus Master Enable.
    let new_cmd = cmd | (1 << 2);
    let new_dw = (cmd_dw & 0xFFFF_0000) | new_cmd as u32;

    config_write32(dev.bus, dev.slot, dev.func, CFG_STATUS_COMMAND, new_dw);
    crate::kprintln!(
        "[PCI]  bus mastering enabled for {:02x}:{:02x}.{}",
        dev.bus,
        dev.slot,
        dev.func,
    );
}
