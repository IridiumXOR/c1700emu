//! A read-only TFTP server (RFC 1350) on top of smoltcp UDP sockets.
//!
//! The protocol in one paragraph: the client sends a read request (RRQ)
//! with a file name to port 69. The server answers *from a new port* (its
//! "transfer ID") with DATA block 1 (512 bytes). The client ACKs each block;
//! the server sends the next one only after the ACK. A block shorter than
//! 512 bytes (possibly empty) ends the transfer. Lost packets are handled by
//! retransmitting the last block after a timeout.
//!
//! Packet encoding/decoding uses the `tftp-packet` crate; this file is only
//! the session logic. Options (RFC 2347 `blksize`...) are ignored, which the
//! RFC allows: clients then fall back to 512-byte blocks.

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::udp;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::IpEndpoint;
use std::path::{Component, Path, PathBuf};
use tftp_packet::{ErrorCode, Packet};

const TFTP_PORT: u16 = 69;
const BLOCK_SIZE: usize = 512;
const RETRANSMIT_AFTER: Duration = Duration::from_millis(1000);
const MAX_RETRIES: u32 = 8;
/// Ephemeral ports used as server transfer IDs.
const FIRST_TID: u16 = 50_000;

fn udp_socket() -> udp::Socket<'static> {
    let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 16 * 600]);
    let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 16 * 600]);
    udp::Socket::new(rx, tx)
}

/// One file being sent to one client.
struct Transfer {
    socket: SocketHandle,
    peer: IpEndpoint,
    name: String,
    data: Vec<u8>,
    /// Block currently "in flight" (sent, waiting for its ACK).
    block: u16,
    /// Absolute index of `block` (block numbers wrap at 65536 for big files).
    block_index: usize,
    sent_at: Instant,
    retries: u32,
    /// Done (or failed). The socket is removed on the *next* service round,
    /// after smoltcp has had a chance to transmit the last queued packet.
    closing: bool,
}

impl Transfer {
    fn payload(&self) -> &[u8] {
        let start = (self.block_index - 1) * BLOCK_SIZE;
        let end = (start + BLOCK_SIZE).min(self.data.len());
        &self.data[start.min(self.data.len())..end]
    }

    fn is_last_block(&self) -> bool {
        self.payload().len() < BLOCK_SIZE
    }

    fn send_current(&mut self, sockets: &mut SocketSet<'static>, now: Instant) {
        let pkt = Packet::DATA { block_number: self.block, data: self.payload().to_vec() }.to_bytes();
        let sock = sockets.get_mut::<udp::Socket>(self.socket);
        if let Err(e) = sock.send_slice(&pkt, self.peer) {
            log::warn!("tftp: send failed: {e}");
        }
        self.sent_at = now;
    }
}

pub struct TftpServer {
    root: Option<PathBuf>,
    listener: SocketHandle,
    transfers: Vec<Transfer>,
    next_tid: u16,
}

impl TftpServer {
    pub fn new(sockets: &mut SocketSet<'static>, root: Option<PathBuf>) -> Self {
        let mut listener = udp_socket();
        listener.bind(TFTP_PORT).expect("bind tftp port");
        TftpServer { root, listener: sockets.add(listener), transfers: Vec::new(), next_tid: FIRST_TID }
    }

    /// Handle received packets and timeouts. Returns true if anything was sent.
    /// A transfer is running (it has retransmission timers to serve).
    pub fn busy(&self) -> bool {
        !self.transfers.is_empty()
    }

    pub fn service(&mut self, sockets: &mut SocketSet<'static>, now: Instant) -> bool {
        let mut sent = false;

        // Retire transfers that finished during the previous round.
        self.transfers.retain(|t| {
            if t.closing {
                sockets.remove(t.socket);
            }
            !t.closing
        });

        // New requests on port 69.
        loop {
            let (request, peer) = {
                let sock = sockets.get_mut::<udp::Socket>(self.listener);
                match sock.recv() {
                    Ok((data, meta)) => (Packet::from_bytes(data), meta.endpoint),
                    Err(_) => break,
                }
            };
            match request {
                Ok(Packet::RRQ { filename, .. }) => {
                    self.start_transfer(sockets, peer, filename, now);
                    sent = true;
                }
                Ok(Packet::WRQ { filename, .. }) => {
                    log::info!("tftp: refusing write of {filename:?} from {peer}");
                    let err = Packet::ERROR { error_code: ErrorCode::AccessViolation, error_msg: "read-only server".into() };
                    let _ = sockets.get_mut::<udp::Socket>(self.listener).send_slice(&err.to_bytes(), peer);
                    sent = true;
                }
                other => log::debug!("tftp: ignoring {other:?} from {peer} on port 69"),
            }
        }

        // ACKs, errors and timeouts of running transfers.
        for t in self.transfers.iter_mut().filter(|t| !t.closing) {
            loop {
                let (pkt, from) = {
                    let sock = sockets.get_mut::<udp::Socket>(t.socket);
                    match sock.recv() {
                        Ok((data, meta)) => (Packet::from_bytes(data), meta.endpoint),
                        Err(_) => break,
                    }
                };
                if from != t.peer {
                    continue; // not our client (RFC 1350 says: send an error; we just ignore)
                }
                match pkt {
                    Ok(Packet::ACK { block_number }) if block_number == t.block => {
                        if t.is_last_block() {
                            log::info!("tftp: sent {} ({} bytes) to {}", t.name, t.data.len(), t.peer);
                            t.closing = true;
                            break;
                        }
                        t.block = t.block.wrapping_add(1);
                        t.block_index += 1;
                        t.retries = 0;
                        t.send_current(sockets, now);
                        sent = true;
                    }
                    Ok(Packet::ACK { .. }) => {} // duplicate ACK: ignore (avoids "sorcerer's apprentice")
                    Ok(Packet::ERROR { error_msg, .. }) => {
                        log::info!("tftp: client aborted {}: {error_msg}", t.name);
                        t.closing = true;
                        break;
                    }
                    other => log::debug!("tftp: unexpected {other:?}"),
                }
            }
            if !t.closing && now - t.sent_at >= RETRANSMIT_AFTER {
                if t.retries >= MAX_RETRIES {
                    log::warn!("tftp: giving up on {} after {MAX_RETRIES} retries", t.name);
                    t.closing = true;
                } else {
                    t.retries += 1;
                    t.send_current(sockets, now);
                    sent = true;
                }
            }
        }

        sent
    }

    fn start_transfer(&mut self, sockets: &mut SocketSet<'static>, peer: IpEndpoint, name: String, now: Instant) {
        let mut sock = udp_socket();
        let tid = self.allocate_tid();
        sock.bind(tid).expect("bind tftp transfer port");
        let socket = sockets.add(sock);

        match self.load(&name) {
            Ok(data) => {
                log::info!("tftp: {peer} reads {name:?} ({} bytes)", data.len());
                let mut t = Transfer {
                    socket,
                    peer,
                    name,
                    data,
                    block: 1,
                    block_index: 1,
                    sent_at: now,
                    retries: 0,
                    closing: false,
                };
                t.send_current(sockets, now);
                self.transfers.push(t);
            }
            Err((code, msg)) => {
                log::info!("tftp: {peer} reads {name:?}: {msg}");
                let err = Packet::ERROR { error_code: code, error_msg: msg }.to_bytes();
                let _ = sockets.get_mut::<udp::Socket>(socket).send_slice(&err, peer);
                // keep the socket until the error is on the wire (closed next round)
                self.transfers.push(Transfer {
                    socket,
                    peer,
                    name: String::new(),
                    data: Vec::new(),
                    block: 0,
                    block_index: 1,
                    sent_at: now,
                    retries: 0,
                    closing: true,
                });
            }
        }
    }

    fn allocate_tid(&mut self) -> u16 {
        let tid = self.next_tid;
        self.next_tid = if self.next_tid == u16::MAX { FIRST_TID } else { self.next_tid + 1 };
        tid
    }

    /// Resolve `name` inside the TFTP root, refusing anything that would
    /// escape it (`..`, absolute paths are taken relative to the root).
    fn load(&self, name: &str) -> Result<Vec<u8>, (ErrorCode, String)> {
        let Some(root) = &self.root else {
            return Err((ErrorCode::AccessViolation, "TFTP disabled: start c1700emu with --tftp-root DIR".into()));
        };
        let mut path = root.clone();
        for comp in Path::new(name).components() {
            match comp {
                Component::Normal(c) => path.push(c),
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir | Component::Prefix(_) => {
                    return Err((ErrorCode::AccessViolation, "illegal file name".into()));
                }
            }
        }
        std::fs::read(&path).map_err(|e| (ErrorCode::FileNotFound, format!("{}: {e}", path.display())))
    }
}
