// kernel/src/net/tcp.rs
//
// TCP — RFC 793. Minimal state machine.
//
// ── Implemented States ───────────────────────────────────────────────────────
//
//   Closed → SynSent → Established → FinWait1 → FinWait2 → TimeWait → Closed
//                                  ↑
//                   Listen → SynReceived ┘  (server side — future)
//
// ── Client Connection Flow ───────────────────────────────────────────────────
//
//   tcp::connect(ip, port)
//     → sends SYN        (seq=ISN)
//     → waits for SYN-ACK (state = SynSent)
//     → sends ACK        (state = Established)
//
//   tcp::send(socket, data)
//     → sends DATA+PSH+ACK segment(s)
//
//   tcp::recv(socket, buf)
//     → waits for data    (spin-poll)
//
//   tcp::close(socket)
//     → sends FIN+ACK    (state = FinWait1)
//     → waits for remote FIN (state = FinWait2 → TimeWait → Closed)

#![allow(dead_code)]
#![allow(static_mut_refs)]

use super::ip::{self, checksum, Ipv4Header, PROTO_TCP};
use super::local_ip;

// ---------------------------------------------------------------------------
// TCP Flags
// ---------------------------------------------------------------------------

pub const TCP_FIN: u8 = 1 << 0;
pub const TCP_SYN: u8 = 1 << 1;
pub const TCP_RST: u8 = 1 << 2;
pub const TCP_PSH: u8 = 1 << 3;
pub const TCP_ACK: u8 = 1 << 4;
pub const TCP_URG: u8 = 1 << 5;

// ---------------------------------------------------------------------------
// TCP Header (20 bytes without options)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct TcpHeader {
    pub src_port: [u8; 2],
    pub dst_port: [u8; 2],
    pub seq: [u8; 4],
    pub ack: [u8; 4],
    /// Data offset (high nibble, in 32-bit words) + reserved + NS.
    pub data_off: u8,
    pub flags: u8,
    pub window: [u8; 2],
    pub checksum: [u8; 2],
    pub urgent: [u8; 2],
}

pub const TCP_HDR_LEN: usize = 20;

impl TcpHeader {
    pub fn seq(&self) -> u32 {
        u32::from_be_bytes(self.seq)
    }
    pub fn ack_nr(&self) -> u32 {
        u32::from_be_bytes(self.ack)
    }
    pub fn data_offset(&self) -> usize {
        ((self.data_off >> 4) as usize) * 4
    }
    pub fn src_port(&self) -> u16 {
        u16::from_be_bytes(self.src_port)
    }
    pub fn dst_port(&self) -> u16 {
        u16::from_be_bytes(self.dst_port)
    }
    pub fn window(&self) -> u16 {
        u16::from_be_bytes(self.window)
    }
}

// ---------------------------------------------------------------------------
// TCP Connection State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    TimeWait,
    CloseWait,
    LastAck,
}

// ---------------------------------------------------------------------------
// TCP Socket
// ---------------------------------------------------------------------------

const MAX_TCP_SOCKETS: usize = 8;
/// Receive buffer size per socket.
const RECV_BUF_SIZE: usize = 4096;

pub struct TcpSocket {
    pub state: TcpState,
    pub local_port: u16,
    pub remote_port: u16,
    pub remote_ip: [u8; 4],
    /// Sequence number sent (next byte to send).
    pub snd_nxt: u32,
    /// Sequence number unacknowledged (next ACK expected from remote).
    pub snd_una: u32,
    /// Sequence number received (next byte expected).
    pub rcv_nxt: u32,
    /// Receive window advertised by remote.
    pub snd_wnd: u16,
    /// Circular receive buffer.
    pub recv_buf: [u8; RECV_BUF_SIZE],
    pub recv_head: usize,
    pub recv_tail: usize,
    pub valid: bool,
}

impl TcpSocket {
    const fn new() -> Self {
        Self {
            state: TcpState::Closed,
            local_port: 0,
            remote_port: 0,
            remote_ip: [0u8; 4],
            snd_nxt: 0,
            snd_una: 0,
            rcv_nxt: 0,
            snd_wnd: 8192,
            recv_buf: [0u8; RECV_BUF_SIZE],
            recv_head: 0,
            recv_tail: 0,
            valid: false,
        }
    }

    /// Deposits data into the receive buffer.
    fn push_recv(&mut self, data: &[u8]) {
        for &b in data {
            let next = (self.recv_head + 1) % RECV_BUF_SIZE;
            if next != self.recv_tail {
                self.recv_buf[self.recv_head] = b;
                self.recv_head = next;
            }
            // If buffer is full, we silently drop (handled by TCP flow control in theory).
        }
    }

    /// Consumes data from the receive buffer.
    fn pop_recv(&mut self, buf: &mut [u8]) -> usize {
        let mut n = 0;
        while n < buf.len() && self.recv_tail != self.recv_head {
            buf[n] = self.recv_buf[self.recv_tail];
            self.recv_tail = (self.recv_tail + 1) % RECV_BUF_SIZE;
            n += 1;
        }
        n
    }

    fn recv_available(&self) -> usize {
        (self.recv_head + RECV_BUF_SIZE - self.recv_tail) % RECV_BUF_SIZE
    }
}

static mut TCP_SOCKETS: [TcpSocket; MAX_TCP_SOCKETS] =
    [const { TcpSocket::new() }; MAX_TCP_SOCKETS];

/// Counter to generate ephemeral local port numbers (49152–65535).
static mut EPHEMERAL_PORT: u16 = 49152;

/// Returns the next ephemeral port.
unsafe fn next_port() -> u16 {
    let p = EPHEMERAL_PORT;
    EPHEMERAL_PORT = if EPHEMERAL_PORT == 65535 {
        49152
    } else {
        EPHEMERAL_PORT + 1
    };
    p
}

/// ISN (Initial Sequence Number) — simplified: we increment a counter.
static mut ISN_COUNTER: u32 = 0x12345678;
unsafe fn next_isn() -> u32 {
    ISN_COUNTER = ISN_COUNTER.wrapping_add(0x00010000);
    ISN_COUNTER
}

// ---------------------------------------------------------------------------
// TCP Segment Construction and Transmission
// ---------------------------------------------------------------------------

/// Sends a TCP segment.
///
/// `data` = optional payload (empty for SYN, ACK, FIN).
///
/// # Safety
/// Calls `ip::send`.
unsafe fn send_segment(sock: &TcpSocket, flags: u8, data: &[u8]) -> Result<(), &'static str> {
    let total = TCP_HDR_LEN + data.len();
    if total > 1460 {
        return Err("tcp: segment too large");
    }

    let mut buf = [0u8; 1480];

    // TCP Header.
    buf[0..2].copy_from_slice(&sock.local_port.to_be_bytes());
    buf[2..4].copy_from_slice(&sock.remote_port.to_be_bytes());
    buf[4..8].copy_from_slice(&sock.snd_nxt.to_be_bytes());
    // ACK number: rcv_nxt if the ACK flag is set.
    let ack_nr = if flags & TCP_ACK != 0 {
        sock.rcv_nxt
    } else {
        0
    };
    buf[8..12].copy_from_slice(&ack_nr.to_be_bytes());
    buf[12] = 0x50; // data offset = 5 (20 bytes)
    buf[13] = flags;
    buf[14..16].copy_from_slice(&8192u16.to_be_bytes()); // window
    buf[16..18].copy_from_slice(&0u16.to_be_bytes()); // checksum = 0
    buf[18..20].copy_from_slice(&0u16.to_be_bytes()); // urgent

    // Data payload.
    if !data.is_empty() {
        buf[TCP_HDR_LEN..TCP_HDR_LEN + data.len()].copy_from_slice(data);
    }

    // Pseudo-header for the TCP checksum (RFC 793 §3.1).
    // pseudo = src_ip(4) + dst_ip(4) + 0(1) + proto(1) + tcp_len(2)
    let src_ip = local_ip();
    let dst_ip = sock.remote_ip;
    let tcp_len = total as u16;

    let mut pseudo = [0u8; 12 + 1480];
    pseudo[0..4].copy_from_slice(&src_ip);
    pseudo[4..8].copy_from_slice(&dst_ip);
    pseudo[8] = 0;
    pseudo[9] = PROTO_TCP;
    pseudo[10..12].copy_from_slice(&tcp_len.to_be_bytes());
    pseudo[12..12 + total].copy_from_slice(&buf[..total]);

    let csum = checksum(&pseudo[..12 + total]);
    buf[16..18].copy_from_slice(&csum.to_be_bytes());

    ip::send(dst_ip, PROTO_TCP, &buf[..total])
}

// ---------------------------------------------------------------------------
// Reception
// ---------------------------------------------------------------------------

/// Processes a received TCP segment.
///
/// Manages state transitions for existing sockets.
///
/// # Safety
/// Reads/writes `static mut TCP_SOCKETS`.
pub unsafe fn handle(ip_hdr: &Ipv4Header, payload: &[u8]) {
    if payload.len() < TCP_HDR_LEN {
        return;
    }

    let hdr = &*(payload.as_ptr() as *const TcpHeader);
    let src_port = hdr.src_port();
    let dst_port = hdr.dst_port();
    let flags = hdr.flags;
    let seq = hdr.seq();
    let ack_nr = hdr.ack_nr();
    let data_off = hdr.data_offset();
    let data = if payload.len() > data_off {
        &payload[data_off..]
    } else {
        &[]
    };

    // Find the corresponding socket.
    for sock in TCP_SOCKETS.iter_mut() {
        if !sock.valid {
            continue;
        }
        if sock.local_port != dst_port {
            continue;
        }
        if sock.remote_port != src_port {
            continue;
        }
        if sock.remote_ip != ip_hdr.src {
            continue;
        }

        match sock.state {
            // ── SYN-ACK received → connection established ────────────────────
            TcpState::SynSent => {
                if flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK) {
                    sock.rcv_nxt = seq.wrapping_add(1);
                    sock.snd_una = ack_nr;
                    sock.state = TcpState::Established;
                    // Send ACK.
                    sock.snd_nxt = ack_nr; // confirm the sent SYN
                    send_segment(sock, TCP_ACK, &[]).ok();
                    crate::kprintln!("[TCP]  connection established (port {})", sock.local_port);
                } else if flags & TCP_RST != 0 {
                    sock.state = TcpState::Closed;
                    crate::kprintln!("[TCP]  connection refused (RST)");
                }
            }

            // ── Data received ────────────────────────────────────────────────
            TcpState::Established => {
                if flags & TCP_RST != 0 {
                    sock.state = TcpState::Closed;
                    return;
                }
                if !data.is_empty() {
                    sock.push_recv(data);
                    sock.rcv_nxt = sock.rcv_nxt.wrapping_add(data.len() as u32);
                    // ACK the received data.
                    send_segment(sock, TCP_ACK, &[]).ok();
                }
                if flags & TCP_FIN != 0 {
                    sock.rcv_nxt = sock.rcv_nxt.wrapping_add(1);
                    sock.state = TcpState::CloseWait;
                    send_segment(sock, TCP_ACK, &[]).ok();
                }
            }

            // ── FIN-ACK received in FinWait1 ─────────────────────────────────
            TcpState::FinWait1 if flags & TCP_ACK != 0 => {
                sock.state = TcpState::FinWait2;
            }

            // ── FIN received in FinWait2 → TimeWait ──────────────────────────
            TcpState::FinWait2 if flags & TCP_FIN != 0 => {
                sock.rcv_nxt = sock.rcv_nxt.wrapping_add(1);
                send_segment(sock, TCP_ACK, &[]).ok();
                sock.state = TcpState::TimeWait;
                // En production : attendre 2×MSL avant de fermer.
                // Ici : fermer immédiatement.
                sock.state = TcpState::Closed;
                sock.valid = false;
                crate::kprintln!("[TCP]  connection closed (port {})", sock.local_port);
            }

            // ── CloseWait: wait for the app to call tcp::close() ─────────────
            TcpState::CloseWait => { /* nothing — app must call close() */ }

            _ => {}
        }
        return;
    }

    // No matching socket found → send RST if it isn't already an RST.
    if flags & TCP_RST == 0 {
        // Fast RST: we don't keep state.
        let _ = ip_hdr; // used in send_segment
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initializes the TCP socket table.
///
/// # Safety
/// Writes to `static mut`.
pub unsafe fn init() {
    for s in TCP_SOCKETS.iter_mut() {
        *s = TcpSocket::new();
    }
    crate::kprintln!("[TCP]  {} sockets initialized", MAX_TCP_SOCKETS);
}

// ---------------------------------------------------------------------------
// TCP Socket API
// ---------------------------------------------------------------------------

/// Opens a TCP connection to `dst_ip:dst_port`.
///
/// Sends a SYN, transitions to SynSent state, waits for SYN-ACK (poll).
/// Returns the socket index or `Err`.
///
/// # Safety
/// Calls `send_segment` and `net::poll`.
pub unsafe fn connect(dst_ip: [u8; 4], dst_port: u16) -> Result<usize, &'static str> {
    // Find a free slot.
    let idx = TCP_SOCKETS
        .iter()
        .position(|s| !s.valid)
        .ok_or("tcp: socket table full")?;

    let sock = &mut TCP_SOCKETS[idx];
    let isn = next_isn();

    sock.local_port = next_port();
    sock.remote_port = dst_port;
    sock.remote_ip = dst_ip;
    sock.snd_nxt = isn;
    sock.snd_una = isn;
    sock.rcv_nxt = 0;
    sock.state = TcpState::SynSent;
    sock.recv_head = 0;
    sock.recv_tail = 0;
    sock.valid = true;

    // Send SYN.
    send_segment(sock, TCP_SYN, &[])?;
    sock.snd_nxt = sock.snd_nxt.wrapping_add(1); // SYN consumes 1 sequence number

    // Wait for SYN-ACK (max ~2000 polls).
    for _ in 0..2000 {
        super::poll();
        if TCP_SOCKETS[idx].state == TcpState::Established {
            crate::kprintln!(
                "[TCP]  connected to {}.{}.{}.{}:{} (local port {})",
                dst_ip[0],
                dst_ip[1],
                dst_ip[2],
                dst_ip[3],
                dst_port,
                TCP_SOCKETS[idx].local_port,
            );
            return Ok(idx);
        }
        if TCP_SOCKETS[idx].state == TcpState::Closed {
            break;
        }
        core::hint::spin_loop();
    }

    TCP_SOCKETS[idx].valid = false;
    Err("tcp: connection timeout")
}

/// Sends data over an established TCP socket.
///
/// Fragments into 1440-byte segments if necessary.
///
/// # Safety
/// Calls `send_segment`.
pub unsafe fn send(socket: usize, data: &[u8]) -> Result<usize, &'static str> {
    if socket >= MAX_TCP_SOCKETS {
        return Err("tcp: invalid socket");
    }
    let sock = &mut TCP_SOCKETS[socket];
    if sock.state != TcpState::Established {
        return Err("tcp: not connected");
    }

    let mss = 1440usize;
    let mut sent = 0usize;

    while sent < data.len() {
        let chunk = &data[sent..(sent + mss).min(data.len())];
        send_segment(sock, TCP_ACK | TCP_PSH, chunk)?;
        sock.snd_nxt = sock.snd_nxt.wrapping_add(chunk.len() as u32);
        sent += chunk.len();
    }
    Ok(sent)
}

/// Receives data from a TCP socket.
///
/// Spin-polls until data arrives.
/// Returns the number of bytes copied into `buf`.
///
/// # Safety
/// Calls `net::poll`.
pub unsafe fn recv(socket: usize, buf: &mut [u8]) -> Result<usize, &'static str> {
    if socket >= MAX_TCP_SOCKETS {
        return Err("tcp: invalid socket");
    }

    loop {
        super::poll();
        let sock = &mut TCP_SOCKETS[socket];
        if sock.recv_available() > 0 {
            return Ok(sock.pop_recv(buf));
        }
        if sock.state == TcpState::Closed {
            return Ok(0);
        }
        core::hint::spin_loop();
    }
}

/// Closes a TCP connection (sends FIN).
///
/// # Safety
/// Calls `send_segment`.
pub unsafe fn close(socket: usize) -> Result<(), &'static str> {
    if socket >= MAX_TCP_SOCKETS {
        return Err("tcp: invalid socket");
    }
    let sock = &mut TCP_SOCKETS[socket];

    match sock.state {
        TcpState::Established | TcpState::CloseWait => {
            send_segment(sock, TCP_FIN | TCP_ACK, &[])?;
            sock.snd_nxt = sock.snd_nxt.wrapping_add(1);
            sock.state = if sock.state == TcpState::CloseWait {
                TcpState::LastAck
            } else {
                TcpState::FinWait1
            };
            crate::kprintln!("[TCP]  FIN sent (port {})", sock.local_port);
        }
        _ => {
            sock.valid = false;
        }
    }
    Ok(())
}
