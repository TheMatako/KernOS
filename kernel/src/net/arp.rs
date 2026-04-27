// kernel/src/net/arp.rs
//
// ARP (Address Resolution Protocol) — RFC 826.
//
// Resolves an IPv4 address to a MAC address on the local network.
// Maintains a 16-entry cache (Simplified LRU: overwrites the oldest).

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::ethernet::{self, BROADCAST_MAC, ETHERTYPE_ARP};
use super::{local_ip, local_mac};

// ---------------------------------------------------------------------------
// ARP Constants
// ---------------------------------------------------------------------------

const HW_TYPE_ETHERNET: u16 = 1;
const PROTO_IPV4: u16 = 0x0800;
const HW_LEN: u8 = 6; // MAC = 6 bytes
const PROTO_LEN: u8 = 4; // IPv4 = 4 bytes
const OP_REQUEST: u16 = 1;
const OP_REPLY: u16 = 2;

// ---------------------------------------------------------------------------
// ARP Packet (28 bytes for Ethernet/IPv4)
// ---------------------------------------------------------------------------

#[repr(C)]
struct ArpPacket {
    hw_type: [u8; 2],    // 1 = Ethernet
    proto_type: [u8; 2], // 0x0800 = IPv4
    hw_len: u8,          // 6
    proto_len: u8,       // 4
    operation: [u8; 2],  // 1 = request, 2 = reply
    sender_mac: [u8; 6],
    sender_ip: [u8; 4],
    target_mac: [u8; 6],
    target_ip: [u8; 4],
}

const ARP_PKT_LEN: usize = core::mem::size_of::<ArpPacket>(); // 28

// ---------------------------------------------------------------------------
// ARP Cache
// ---------------------------------------------------------------------------

const ARP_CACHE_SIZE: usize = 16;

#[derive(Clone, Copy, Default)]
struct ArpEntry {
    ip: [u8; 4],
    mac: [u8; 6],
    valid: bool,
}

static mut ARP_CACHE: [ArpEntry; ARP_CACHE_SIZE] = [ArpEntry {
    ip: [0u8; 4],
    mac: [0u8; 6],
    valid: false,
}; ARP_CACHE_SIZE];

/// Index of the next entry to overwrite (round-robin).
static mut ARP_NEXT: usize = 0;

/// Initializes the ARP cache.
///
/// # Safety
/// Writes to `static mut`.
pub unsafe fn init() {
    for e in ARP_CACHE.iter_mut() {
        *e = ArpEntry::default();
    }
    crate::kprintln!("[ARP]  cache initialized ({} entries)", ARP_CACHE_SIZE);
}

/// Looks up an `ip` in the cache. Returns the MAC or `None`.
pub fn lookup(ip: [u8; 4]) -> Option<[u8; 6]> {
    unsafe {
        for e in &ARP_CACHE {
            if e.valid && e.ip == ip {
                return Some(e.mac);
            }
        }
        None
    }
}

/// Inserts or updates an entry in the cache.
///
/// # Safety
/// Writes to `static mut ARP_CACHE`.
pub unsafe fn insert(ip: [u8; 4], mac: [u8; 6]) {
    // Update if already present.
    for e in ARP_CACHE.iter_mut() {
        if e.valid && e.ip == ip {
            e.mac = mac;
            return;
        }
    }
    // Otherwise overwrite the next entry (round-robin).
    let idx = ARP_NEXT;
    ARP_CACHE[idx] = ArpEntry {
        ip,
        mac,
        valid: true,
    };
    ARP_NEXT = (idx + 1) % ARP_CACHE_SIZE;
}

// ---------------------------------------------------------------------------
// ARP Reception
// ---------------------------------------------------------------------------

/// Processes a received ARP packet.
///
/// - ARP Request for our IP → send an ARP Reply.
/// - ARP Reply → update the cache.
///
/// # Safety
/// Calls `ethernet::send`.
pub unsafe fn handle(payload: &[u8]) {
    if payload.len() < ARP_PKT_LEN {
        return;
    }

    let pkt = &*(payload.as_ptr() as *const ArpPacket);

    // Verify it's a valid Ethernet/IPv4 ARP packet.
    if u16::from_be_bytes(pkt.hw_type) != HW_TYPE_ETHERNET {
        return;
    }
    if u16::from_be_bytes(pkt.proto_type) != PROTO_IPV4 {
        return;
    }
    if pkt.hw_len != HW_LEN {
        return;
    }
    if pkt.proto_len != PROTO_LEN {
        return;
    }

    let op = u16::from_be_bytes(pkt.operation);
    let sender_ip = pkt.sender_ip;
    let sender_mac = pkt.sender_mac;

    // Always cache the sender's MAC.
    insert(sender_ip, sender_mac);

    match op {
        OP_REQUEST => {
            // Is it addressed to us?
            if pkt.target_ip != local_ip() {
                return;
            }

            // Send an ARP Reply.
            let reply = ArpPacket {
                hw_type: HW_TYPE_ETHERNET.to_be_bytes(),
                proto_type: PROTO_IPV4.to_be_bytes(),
                hw_len: HW_LEN,
                proto_len: PROTO_LEN,
                operation: OP_REPLY.to_be_bytes(),
                sender_mac: local_mac(),
                sender_ip: local_ip(),
                target_mac: sender_mac,
                target_ip: sender_ip,
            };

            let bytes: &[u8] =
                core::slice::from_raw_parts(&reply as *const ArpPacket as *const u8, ARP_PKT_LEN);
            ethernet::send(sender_mac, ETHERTYPE_ARP, bytes).ok();
            crate::kprintln!(
                "[ARP]  reply → {}.{}.{}.{}",
                sender_ip[0],
                sender_ip[1],
                sender_ip[2],
                sender_ip[3]
            );
        }
        OP_REPLY => {
            crate::kprintln!(
                "[ARP]  learned {}.{}.{}.{} → {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                sender_ip[0],
                sender_ip[1],
                sender_ip[2],
                sender_ip[3],
                sender_mac[0],
                sender_mac[1],
                sender_mac[2],
                sender_mac[3],
                sender_mac[4],
                sender_mac[5],
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Transmitting an ARP Request
// ---------------------------------------------------------------------------

/// Sends an ARP Request to resolve `target_ip`.
///
/// The reply will arrive asynchronously during the next `net::poll()`.
///
/// # Safety
/// Calls `ethernet::send`.
pub unsafe fn request(target_ip: [u8; 4]) {
    let pkt = ArpPacket {
        hw_type: HW_TYPE_ETHERNET.to_be_bytes(),
        proto_type: PROTO_IPV4.to_be_bytes(),
        hw_len: HW_LEN,
        proto_len: PROTO_LEN,
        operation: OP_REQUEST.to_be_bytes(),
        sender_mac: local_mac(),
        sender_ip: local_ip(),
        target_mac: [0u8; 6],
        target_ip,
    };
    let bytes: &[u8] =
        core::slice::from_raw_parts(&pkt as *const ArpPacket as *const u8, ARP_PKT_LEN);
    ethernet::send(BROADCAST_MAC, ETHERTYPE_ARP, bytes).ok();
}

/// Resolves `ip` → MAC: checks the cache, sends a request if missing.
///
/// Spin-waits up to 1000 poll iterations for the cache to be populated.
///
/// # Safety
/// Calls `request` and `net::poll`.
pub unsafe fn resolve(ip: [u8; 4]) -> Option<[u8; 6]> {
    if let Some(mac) = lookup(ip) {
        return Some(mac);
    }

    request(ip);

    for _ in 0..1000 {
        super::poll();
        if let Some(mac) = lookup(ip) {
            return Some(mac);
        }
        core::hint::spin_loop();
    }
    None
}
