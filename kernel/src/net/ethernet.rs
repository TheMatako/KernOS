// kernel/src/net/ethernet.rs
//
// Ethernet II Layer.
//
// An Ethernet II frame = [dst_mac(6) | src_mac(6) | ethertype(2) | payload]
// The CRC (4 bytes) is appended/verified by the hardware NIC, never seen by the driver.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::{arp, ip, local_mac};
use crate::drivers::e1000;

// ---------------------------------------------------------------------------
// Known Ethertypes
// ---------------------------------------------------------------------------

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86DD; // Future use

/// Broadcast MAC address.
pub const BROADCAST_MAC: [u8; 6] = [0xFF; 6];

// ---------------------------------------------------------------------------
// Ethernet Frame (Memory Layout)
// ---------------------------------------------------------------------------

/// Ethernet II Header (14 bytes).
#[repr(C)]
pub struct EtherHeader {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    /// Ethertype in big-endian format.
    pub ethertype: [u8; 2],
}

impl EtherHeader {
    /// Returns the ethertype in host byte order.
    pub fn ethertype(&self) -> u16 {
        u16::from_be_bytes(self.ethertype)
    }
}

pub const ETHER_HDR_LEN: usize = core::mem::size_of::<EtherHeader>(); // 14

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Parses a received frame and dispatches it to ARP or IPv4.
///
/// Silently ignores frames that are not destined for us or have
/// an unknown ethertype.
///
/// # Safety
/// Calls network handlers that may transmit packets.
pub unsafe fn dispatch(frame: &[u8]) {
    if frame.len() < ETHER_HDR_LEN {
        return;
    }

    let hdr = &*(frame.as_ptr() as *const EtherHeader);
    let payload = &frame[ETHER_HDR_LEN..];

    // Filter: accept only frames addressed to us or broadcast.
    let our_mac = local_mac();
    let dst = hdr.dst;
    if dst != our_mac && dst != BROADCAST_MAC {
        return;
    }

    match hdr.ethertype() {
        ETHERTYPE_ARP => arp::handle(payload),
        ETHERTYPE_IPV4 => ip::handle(payload),
        _ => { /* Unknown ethertype — ignore */ }
    }
}

// ---------------------------------------------------------------------------
// Transmission
// ---------------------------------------------------------------------------

/// Sends an Ethernet frame.
///
/// Constructs the header, copies the payload, and invokes `e1000::send`.
///
/// # Safety
/// Calls `e1000::send`.
pub unsafe fn send(dst_mac: [u8; 6], ethertype: u16, payload: &[u8]) -> Result<(), &'static str> {
    // Stack buffer: header (14) + payload (max ~1500).
    let total = ETHER_HDR_LEN + payload.len();
    if total > 2048 {
        return Err("ethernet: frame too large");
    }

    let mut buf = [0u8; 2048];
    let src_mac = local_mac();

    // Destination MAC.
    buf[0..6].copy_from_slice(&dst_mac);
    // Source MAC.
    buf[6..12].copy_from_slice(&src_mac);
    // Ethertype (big-endian).
    buf[12..14].copy_from_slice(&ethertype.to_be_bytes());
    // Payload.
    buf[14..14 + payload.len()].copy_from_slice(payload);

    e1000::send(&buf[..total])
}
