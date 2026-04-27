// kernel/src/net/icmp.rs
//
// ICMP — RFC 792. Only Echo Reply (responding to ping) is implemented.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::ip::{self, Ipv4Header, PROTO_ICMP};

const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

/// Processes a received ICMP packet.
///
/// If it's an Echo Request (ping) for us → send an Echo Reply.
///
/// # Safety
/// Calls `ip::send`.
pub unsafe fn handle(ip_hdr: &Ipv4Header, payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }

    let icmp_type = payload[0];
    let icmp_code = payload[1];

    if icmp_type != ICMP_ECHO_REQUEST || icmp_code != 0 {
        return;
    }

    // Construct the Echo Reply: same payload, type = 0.
    let mut reply = [0u8; 2048];
    let len = payload.len().min(2040);
    reply[..len].copy_from_slice(&payload[..len]);

    // Change the type.
    reply[0] = ICMP_ECHO_REPLY;
    // Clear the checksum before recalculating.
    reply[2] = 0;
    reply[3] = 0;
    // Recalculate ICMP checksum.
    let csum = ip::checksum(&reply[..len]);
    reply[2..4].copy_from_slice(&csum.to_be_bytes());

    // Send back to the origin.
    ip::send(ip_hdr.src, PROTO_ICMP, &reply[..len]).ok();

    crate::kprintln!(
        "[ICMP] echo reply → {}.{}.{}.{}",
        ip_hdr.src[0],
        ip_hdr.src[1],
        ip_hdr.src[2],
        ip_hdr.src[3],
    );
}
