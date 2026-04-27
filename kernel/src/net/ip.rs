// kernel/src/net/ip.rs
//
// IPv4 Layer — RFC 791.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::ethernet::ETHERTYPE_IPV4;
use super::{arp, ethernet, icmp, local_ip, tcp, udp};

// ---------------------------------------------------------------------------
// IP Protocols
// ---------------------------------------------------------------------------

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

// ---------------------------------------------------------------------------
// IPv4 Header (20 bytes without options)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct Ipv4Header {
    /// Version (4) + IHL (header length in 32-bit words, min 5).
    pub ver_ihl: u8,
    /// DSCP + ECN (ignored).
    pub dscp_ecn: u8,
    /// Total length (header + payload) in bytes, big-endian.
    pub total_len: [u8; 2],
    /// Identification (for fragmentation — unused here).
    pub id: [u8; 2],
    /// Flags + Fragment Offset (we don't fragment).
    pub flags_frag: [u8; 2],
    /// Time To Live (TTL).
    pub ttl: u8,
    /// Protocol (1=ICMP, 6=TCP, 17=UDP).
    pub protocol: u8,
    /// Header checksum (big-endian).
    pub checksum: [u8; 2],
    /// Source address.
    pub src: [u8; 4],
    /// Destination address.
    pub dst: [u8; 4],
}

pub const IPV4_HDR_LEN: usize = 20;

impl Ipv4Header {
    pub fn total_len(&self) -> u16 {
        u16::from_be_bytes(self.total_len)
    }
    pub fn ihl_bytes(&self) -> usize {
        ((self.ver_ihl & 0x0F) as usize) * 4
    }
    pub fn protocol(&self) -> u8 {
        self.protocol
    }
}

// ---------------------------------------------------------------------------
// Internet Checksum (RFC 1071)
// ---------------------------------------------------------------------------

/// Computes the Internet checksum over `data`.
///
/// Used for IPv4, ICMP, TCP, and UDP headers.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    // Remaining byte (odd length).
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    // Fold carries.
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Processes a received IPv4 datagram (after the Ethernet header).
///
/// # Safety
/// Calls icmp/udp/tcp handlers.
pub unsafe fn handle(payload: &[u8]) {
    if payload.len() < IPV4_HDR_LEN {
        return;
    }

    let hdr = &*(payload.as_ptr() as *const Ipv4Header);
    let ihl = hdr.ihl_bytes();
    if ihl < IPV4_HDR_LEN || payload.len() < ihl {
        return;
    }

    // Verify it's destined for us (or broadcast).
    let our_ip = local_ip();
    let bcast = [255u8, 255, 255, 255];
    if hdr.dst != our_ip && hdr.dst != bcast {
        return;
    }

    // Verify the header checksum.
    if checksum(&payload[..ihl]) != 0 {
        return;
    } // Invalid checksum

    let total = hdr.total_len() as usize;
    let transport = &payload[ihl..total.min(payload.len())];

    match hdr.protocol() {
        PROTO_ICMP => icmp::handle(hdr, transport),
        PROTO_UDP => udp::handle(hdr, transport),
        PROTO_TCP => tcp::handle(hdr, transport),
        _ => {} // Unknown protocol
    }
}

// ---------------------------------------------------------------------------
// Transmission
// ---------------------------------------------------------------------------

/// Sends an IPv4 datagram.
///
/// Resolves the destination MAC via ARP (uses gateway if outside local subnet).
/// Constructs the IPv4 header, calculates the checksum, and sends via Ethernet.
///
/// # Safety
/// Calls `arp::resolve` and `ethernet::send`.
pub unsafe fn send(dst_ip: [u8; 4], proto: u8, payload: &[u8]) -> Result<(), &'static str> {
    // Decide whether to route locally or via the gateway.
    let our_ip = local_ip();
    let netmask = super::NETMASK;
    let is_local = (dst_ip[0] & netmask[0]) == (our_ip[0] & netmask[0])
        && (dst_ip[1] & netmask[1]) == (our_ip[1] & netmask[1])
        && (dst_ip[2] & netmask[2]) == (our_ip[2] & netmask[2])
        && (dst_ip[3] & netmask[3]) == (our_ip[3] & netmask[3]);

    let next_hop = if is_local {
        dst_ip
    } else {
        super::gateway_ip()
    };

    let dst_mac = arp::resolve(next_hop).ok_or("ip: ARP resolution failed")?;

    // Build the IP buffer (header + payload).
    let total = IPV4_HDR_LEN + payload.len();
    if total > 2000 {
        return Err("ip: packet too large");
    }

    let mut buf = [0u8; 2048];

    // Populate the header.
    buf[0] = 0x45; // version=4, IHL=5
    buf[1] = 0; // DSCP/ECN
    buf[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    buf[4..6].copy_from_slice(&0u16.to_be_bytes()); // ID = 0
    buf[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF bit (Don't Fragment)
    buf[8] = 64; // TTL
    buf[9] = proto;
    buf[10..12].copy_from_slice(&0u16.to_be_bytes()); // checksum = 0 initially
    buf[12..16].copy_from_slice(&our_ip);
    buf[16..20].copy_from_slice(&dst_ip);

    // Calculate header checksum.
    let csum = checksum(&buf[..IPV4_HDR_LEN]);
    buf[10..12].copy_from_slice(&csum.to_be_bytes());

    // Copy the payload.
    buf[IPV4_HDR_LEN..IPV4_HDR_LEN + payload.len()].copy_from_slice(payload);

    ethernet::send(dst_mac, ETHERTYPE_IPV4, &buf[..total])
}
