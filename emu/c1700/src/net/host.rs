//! Real network endpoints for the router's Ethernet port (docs ch. 18):
//!
//! * a Linux **TAP** device: the host sees an Ethernet interface (say
//!   `tap0`) and can give it an address, bridge it to a physical NIC or NAT
//!   it, so the router reaches the real LAN;
//! * a **UDP tunnel**: every Ethernet frame is one UDP datagram, the "UDP
//!   NIO" of Dynamips and GNS3. Two emulators (or an emulator and GNS3) can
//!   be cabled together without any privileges.
//!
//! Frames from the guest go out with one system call. Frames for the guest
//! are received by a background thread that blocks on the device and hands
//! them over through a bounded channel, so the emulation loop only does a
//! cheap `try_recv` when it polls; the thread also wakes the loop if it is
//! sleeping through idle time ([`Wakeup`]).

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;

use crate::wakeup::Wakeup;

/// Frames waiting for the guest; beyond this the newest are dropped (a
/// guest that stopped receiving must not make us buffer forever).
const MAX_QUEUED_FRAMES: usize = 256;
/// Largest frame we read (Ethernet 1514 without FCS, VLAN tag, slack).
const MAX_FRAME: usize = 2048;

pub struct HostLink {
    /// For messages: "TAP device tap0", "UDP 10001 -> 127.0.0.1:10002".
    pub what: String,
    send: Box<dyn FnMut(&[u8]) -> io::Result<usize> + Send>,
    rx: Receiver<Vec<u8>>,
    queue: VecDeque<Vec<u8>>,
    /// Send errors so far (only the first few are logged).
    send_errors: u64,
}

impl HostLink {
    /// Open the existing TAP device `name`. Creating one needs root, so it
    /// is created beforehand, owned by the user (docs ch. 18.2):
    /// `sudo ip tuntap add dev tap0 mode tap user $USER`.
    pub fn tap(name: &str, wakeup: Wakeup) -> io::Result<Self> {
        let iface = tun_tap::Iface::without_packet_info(name, tun_tap::Mode::Tap)
            .map_err(|e| io::Error::new(e.kind(), format!("cannot open TAP device {name}: {e}")))?;
        let iface = Arc::new(iface);
        let reader = Arc::clone(&iface);
        let what = format!("TAP device {}", iface.name());
        let rx = spawn_receiver(&what, wakeup, move |buf| reader.recv(buf))?;
        Ok(HostLink { what, send: Box::new(move |frame| iface.send(frame)), rx, queue: VecDeque::new(), send_errors: 0 })
    }

    /// A UDP tunnel from local port `local_port` to `remote`.
    pub fn udp(local_port: u16, remote: SocketAddr, wakeup: Wakeup) -> io::Result<Self> {
        let bind: SocketAddr = if remote.is_ipv6() { ([0u16; 8], local_port).into() } else { ([0, 0, 0, 0], local_port).into() };
        let socket = UdpSocket::bind(bind).map_err(|e| io::Error::new(e.kind(), format!("cannot bind UDP port {local_port}: {e}")))?;
        socket.connect(remote)?;
        let reader = socket.try_clone()?;
        let what = format!("UDP {local_port} -> {remote}");
        let rx = spawn_receiver(&what, wakeup, move |buf| loop {
            match reader.recv(buf) {
                // A connected UDP socket reports "connection refused" when the
                // other end isn't listening yet (ICMP port unreachable): wait on.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => continue,
                other => return other,
            }
        })?;
        Ok(HostLink { what, send: Box::new(move |frame| socket.send(frame)), rx, queue: VecDeque::new(), send_errors: 0 })
    }

    /// A frame the guest put on the wire (without FCS).
    pub fn guest_transmit(&mut self, frame: Vec<u8>) {
        log::debug!(target: "net", "guest -> {}  {}", self.what, super::describe(&frame));
        if let Err(e) = (self.send)(&frame) {
            self.send_errors += 1;
            // Nobody listening at the other end of a UDP tunnel is normal
            // until it starts; log the first few failures only.
            if self.send_errors <= 3 {
                log::warn!(target: "net", "{}: send failed: {e}", self.what);
            }
        }
    }

    pub fn peek_for_guest(&self) -> Option<&Vec<u8>> {
        self.queue.front()
    }

    pub fn pop_for_guest(&mut self) -> Option<Vec<u8>> {
        let frame = self.queue.pop_front()?;
        log::debug!(target: "net", "{} -> guest  {}", self.what, super::describe(&frame));
        Some(frame)
    }

    /// Collect what the receiver thread got.
    pub fn poll(&mut self) {
        while self.queue.len() < MAX_QUEUED_FRAMES {
            match self.rx.try_recv() {
                Ok(frame) => self.queue.push_back(frame),
                Err(_) => break,
            }
        }
    }
}

/// Start a thread that calls `recv` in a loop and forwards each frame.
fn spawn_receiver(
    what: &str,
    wakeup: Wakeup,
    mut recv: impl FnMut(&mut [u8]) -> io::Result<usize> + Send + 'static,
) -> io::Result<Receiver<Vec<u8>>> {
    let (tx, rx): (SyncSender<Vec<u8>>, _) = sync_channel(MAX_QUEUED_FRAMES);
    let name = what.to_string();
    thread::Builder::new().name(format!("rx {what}")).spawn(move || {
        let mut buf = vec![0u8; MAX_FRAME];
        loop {
            match recv(&mut buf) {
                Ok(n) => match tx.try_send(buf[..n].to_vec()) {
                    Ok(()) => wakeup.notify(),
                    Err(TrySendError::Full(_)) => {} // full: drop, like a busy NIC
                    Err(TrySendError::Disconnected(_)) => return, // the emulator is gone
                },
                Err(e) => {
                    log::error!(target: "net", "{name}: receive failed, link down: {e}");
                    return;
                }
            }
        }
    })?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Two ends of a UDP tunnel on localhost: a frame sent on one arrives
    /// on the other, unchanged.
    #[test]
    fn udp_tunnel_carries_frames_both_ways() {
        let a_port = 40_000 + (std::process::id() % 10_000) as u16;
        let b_port = a_port + 1;
        let mut a = HostLink::udp(a_port, ([127, 0, 0, 1], b_port).into(), Wakeup::default()).unwrap();
        let mut b = HostLink::udp(b_port, ([127, 0, 0, 1], a_port).into(), Wakeup::default()).unwrap();
        let frame: Vec<u8> = (0..60u8).collect();
        a.guest_transmit(frame.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while b.peek_for_guest().is_none() && Instant::now() < deadline {
            b.poll();
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(b.pop_for_guest(), Some(frame.clone()));
        b.guest_transmit(frame[..42].to_vec());
        while a.peek_for_guest().is_none() && Instant::now() < deadline {
            a.poll();
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(a.pop_for_guest(), Some(frame[..42].to_vec()));
    }
}
