// kernel/src/net/mod.rs
//
// KernOS Network Stack — Brick 9.
//
// Implemented layers (bottom → top):
//   Ethernet  (net/ethernet.rs) — frames, parsing, sending
//   ARP       (net/arp.rs)      — MAC ↔ IP resolution, caching
//   IPv4      (net/ip.rs)       — parsing, routing, checksums
//   ICMP      (net/icmp.rs)     — echo request/reply (ping)
//   UDP       (net/udp.rs)      — sending/receiving datagrams
//   TCP       (net/tcp.rs)      — state machine, connect/send/recv/close

#![allow(dead_code)]
#![allow(static_mut_refs)]

pub mod arp;
pub mod ethernet;
pub mod icmp;
pub mod ip;
pub mod tcp;
pub mod udp;

// ---------------------------------------------------------------------------
// Static Network Configuration
// ---------------------------------------------------------------------------

/// IPv4 address of this machine (configurable at boot).
/// Format: [a, b, c, d] → a.b.c.d
static mut LOCAL_IP: [u8; 4] = [10, 0, 2, 15]; // QEMU user-net default
static mut GATEWAY_IP: [u8; 4] = [10, 0, 2, 2]; // QEMU gateway
static mut NETMASK: [u8; 4] = [255, 255, 255, 0];
static mut LOCAL_MAC: [u8; 6] = [0u8; 6]; // Populated by init()

/// Returns the local IP address.
pub fn local_ip() -> [u8; 4] {
    unsafe { LOCAL_IP }
}

/// Returns the local MAC address.
pub fn local_mac() -> [u8; 6] {
    unsafe { LOCAL_MAC }
}

/// Returns the gateway IP address.
pub fn gateway_ip() -> [u8; 4] {
    unsafe { GATEWAY_IP }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initializes the network stack.
///
/// Must be called after `e1000::init()`.
///
/// # Safety
/// Writes to `static mut` state.
pub unsafe fn init() {
    // Retrieve the MAC address from the NIC.
    LOCAL_MAC = crate::drivers::e1000::mac_address();

    crate::kprintln!(
        "[NET]  init — IP={}.{}.{}.{}  GW={}.{}.{}.{}  MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        LOCAL_IP[0], LOCAL_IP[1], LOCAL_IP[2], LOCAL_IP[3],
        GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3],
        LOCAL_MAC[0], LOCAL_MAC[1], LOCAL_MAC[2], LOCAL_MAC[3], LOCAL_MAC[4], LOCAL_MAC[5],
    );

    // Initialize submodules.
    arp::init();
    tcp::init();
}

// ---------------------------------------------------------------------------
// Main Polling Loop
// ---------------------------------------------------------------------------

/// Receive pump: reads a packet from the NIC and dispatches it
/// through the network stack.
///
/// Returns `true` if a packet was processed, `false` if the ring was empty.
///
/// To be called from a dedicated network task or the tick scheduler.
///
/// # Safety
/// Calls `e1000::recv` and network submodules.
pub unsafe fn poll() -> bool {
    let mut buf = [0u8; 2048];
    let len = match crate::drivers::e1000::recv(&mut buf) {
        Ok(0) => return false,
        Ok(n) => n,
        Err(_) => return false,
    };

    ethernet::dispatch(&buf[..len]);
    true
}
