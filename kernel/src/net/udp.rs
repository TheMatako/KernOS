// kernel/src/net/udp.rs
//
// UDP — RFC 768. Connectionless datagrams.

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::ip::{self, Ipv4Header, PROTO_UDP};

// ---------------------------------------------------------------------------
// UDP Header (8 bytes)
// ---------------------------------------------------------------------------

#[repr(C)]
struct UdpHeader {
    src_port: [u8; 2],
    dst_port: [u8; 2],
    length: [u8; 2], // header + data
    checksum: [u8; 2],
}

const UDP_HDR_LEN: usize = 8;

// ---------------------------------------------------------------------------
// UDP Socket Table
// ---------------------------------------------------------------------------

const MAX_UDP_SOCKETS: usize = 8;

/// A UDP socket bound to a local port.
#[derive(Clone, Copy)]
pub struct UdpSocket {
    pub local_port: u16,
    /// Ring buffer for received packets (simplified: max 1 packet waiting).
    pub recv_buf: [u8; 1500],
    pub recv_len: usize,
    pub recv_src_ip: [u8; 4],
    pub recv_src_port: u16,
    pub valid: bool,
}

static mut UDP_SOCKETS: [UdpSocket; MAX_UDP_SOCKETS] = [UdpSocket {
    local_port: 0,
    recv_buf: [0u8; 1500],
    recv_len: 0,
    recv_src_ip: [0u8; 4],
    recv_src_port: 0,
    valid: false,
}; MAX_UDP_SOCKETS];

// ---------------------------------------------------------------------------
// Reception
// ---------------------------------------------------------------------------

/// Processes a received UDP datagram.
///
/// Looks for a socket bound to the destination port and deposits the payload.
///
/// # Safety
/// Reads/writes `static mut UDP_SOCKETS`.
pub unsafe fn handle(ip_hdr: &Ipv4Header, payload: &[u8]) {
    if payload.len() < UDP_HDR_LEN {
        return;
    }

    let hdr = &*(payload.as_ptr() as *const UdpHeader);
    let dst_port = u16::from_be_bytes(hdr.dst_port);
    let src_port = u16::from_be_bytes(hdr.src_port);
    let data_len = (u16::from_be_bytes(hdr.length) as usize).saturating_sub(UDP_HDR_LEN);
    let data = &payload[UDP_HDR_LEN..UDP_HDR_LEN + data_len.min(payload.len() - UDP_HDR_LEN)];

    for sock in UDP_SOCKETS.iter_mut() {
        if sock.valid && sock.local_port == dst_port {
            let copy = data.len().min(sock.recv_buf.len());
            sock.recv_buf[..copy].copy_from_slice(&data[..copy]);
            sock.recv_len = copy;
            sock.recv_src_ip = ip_hdr.src;
            sock.recv_src_port = src_port;
            return;
        }
    }
    // No matching socket → silently ignore.
}

// ---------------------------------------------------------------------------
// UDP Socket API
// ---------------------------------------------------------------------------

/// Opens a UDP socket bound to `local_port`.
///
/// Returns the socket index, or `Err` if the table is full.
///
/// # Safety
/// Writes to `static mut UDP_SOCKETS`.
pub unsafe fn bind(local_port: u16) -> Result<usize, &'static str> {
    for (i, sock) in UDP_SOCKETS.iter_mut().enumerate() {
        if !sock.valid {
            sock.local_port = local_port;
            sock.recv_len = 0;
            sock.valid = true;
            return Ok(i);
        }
    }
    Err("udp: socket table full")
}

/// Closes a UDP socket.
///
/// # Safety
/// Writes to `static mut UDP_SOCKETS`.
pub unsafe fn close(socket: usize) {
    if socket < MAX_UDP_SOCKETS {
        UDP_SOCKETS[socket].valid = false;
    }
}

/// Sends a UDP datagram.
///
/// # Safety
/// Calls `ip::send`.
pub unsafe fn send(
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    data: &[u8],
) -> Result<(), &'static str> {
    let total = UDP_HDR_LEN + data.len();
    if total > 1500 {
        return Err("udp: payload too large");
    }

    let mut buf = [0u8; 1508];

    // UDP Header.
    buf[0..2].copy_from_slice(&src_port.to_be_bytes());
    buf[2..4].copy_from_slice(&dst_port.to_be_bytes());
    buf[4..6].copy_from_slice(&(total as u16).to_be_bytes());
    buf[6..8].copy_from_slice(&0u16.to_be_bytes()); // optional checksum in UDP/IPv4

    // Data.
    buf[UDP_HDR_LEN..UDP_HDR_LEN + data.len()].copy_from_slice(data);

    ip::send(dst_ip, PROTO_UDP, &buf[..total])
}

/// Blocking read on a UDP socket.
///
/// Spin-polls until a packet arrives on the given `socket`.
/// Copies the payload into `buf` and returns `(byte_count, src_ip, src_port)`.
///
/// # Safety
/// Calls `net::poll` and reads `static mut UDP_SOCKETS`.
pub unsafe fn recv(socket: usize, buf: &mut [u8]) -> Result<(usize, [u8; 4], u16), &'static str> {
    if socket >= MAX_UDP_SOCKETS || !UDP_SOCKETS[socket].valid {
        return Err("udp: invalid socket");
    }
    loop {
        super::poll();
        let sock = &mut UDP_SOCKETS[socket];
        if sock.recv_len > 0 {
            let n = sock.recv_len.min(buf.len());
            buf[..n].copy_from_slice(&sock.recv_buf[..n]);
            let src_ip = sock.recv_src_ip;
            let src_port = sock.recv_src_port;
            sock.recv_len = 0;
            return Ok((n, src_ip, src_port));
        }
        core::hint::spin_loop();
    }
}
