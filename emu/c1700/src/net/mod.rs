//! Built-in virtual network ("user-mode networking"), so the guest can use
//! `tftpdnld` without root privileges or a TAP device.
//!
//! The guest's Ethernet port is plugged into a virtual cable whose other end
//! is a tiny virtual host implemented with the `smoltcp` TCP/IP stack:
//!
//! ```text
//!   guest FEC  ⇄  Cable (two frame queues)  ⇄  smoltcp Interface
//!                                               ├─ answers ARP and ping for
//!                                               │  the server and gateway IPs
//!                                               └─ UDP sockets → TftpServer
//! ```
//!
//! Defaults follow QEMU's user networking: server 10.0.2.2, gateway
//! 10.0.2.1, netmask 255.255.255.0 — give the router e.g. 10.0.2.15.
//! Time is the emulator's virtual time, so retransmission timers on both
//! sides agree no matter how fast the host is.

pub mod host;
pub mod tftp;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{
    ArpPacket, EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr, IpProtocol,
    Ipv4Address, Ipv4Packet, UdpPacket,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use tftp::TftpServer;

#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Address of the virtual host (the TFTP server).
    pub server_ip: Ipv4Address,
    /// Second address answered by the virtual host, to be used as DEFAULT_GATEWAY.
    pub gateway_ip: Ipv4Address,
    pub prefix_len: u8,
    /// Directory served over TFTP (`None` = every request gets an error).
    pub tftp_root: Option<PathBuf>,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            server_ip: Ipv4Address::new(10, 0, 2, 2),
            gateway_ip: Ipv4Address::new(10, 0, 2, 1),
            prefix_len: 24,
            tftp_root: None,
        }
    }
}

/// MAC address of the virtual host (locally administered, like QEMU's 52:55:...).
const HOST_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];
/// Don't let a guest that stopped receiving make us buffer forever.
const MAX_QUEUED_FRAMES: usize = 256;

/// The virtual Ethernet cable between the guest's FEC and smoltcp.
#[derive(Default)]
pub struct Cable {
    /// Frames transmitted by the guest, waiting for smoltcp.
    from_guest: VecDeque<Vec<u8>>,
    /// Frames produced by smoltcp, waiting for the guest's receive ring.
    to_guest: VecDeque<Vec<u8>>,
}

impl Device for Cable {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514; // Ethernet frame without FCS
        caps
    }

    fn receive(&mut self, _now: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.from_guest.pop_front()?;
        Some((RxToken { frame }, TxToken { queue: &mut self.to_guest }))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        Some(TxToken { queue: &mut self.to_guest })
    }
}

pub struct RxToken {
    frame: Vec<u8>,
}

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame)
    }
}

pub struct TxToken<'a> {
    queue: &'a mut VecDeque<Vec<u8>>,
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0; len];
        let r = f(&mut frame);
        if self.queue.len() < MAX_QUEUED_FRAMES {
            self.queue.push_back(frame);
        }
        r
    }
}

pub struct VirtualNetwork {
    cable: Cable,
    iface: Interface,
    sockets: SocketSet<'static>,
    tftp: TftpServer,
    pub config: NetConfig,
    /// Virtual time of the next housekeeping poll when nothing happens.
    next_idle_poll_us: u64,
}

/// Housekeeping poll period while the network is quiet (20 ms virtual).
/// Polling smoltcp every 4096 cycles regardless cost ~7% of host time.
const IDLE_POLL_US: u64 = 20_000;

impl VirtualNetwork {
    pub fn new(config: NetConfig) -> Self {
        let mut cable = Cable::default();
        let hw = HardwareAddress::Ethernet(EthernetAddress(HOST_MAC));
        let mut iface = Interface::new(Config::new(hw), &mut cable, Instant::ZERO);
        iface.update_ip_addrs(|addrs| {
            for ip in [config.server_ip, config.gateway_ip] {
                addrs.push(IpCidr::new(IpAddress::Ipv4(ip), config.prefix_len)).expect("too many addresses");
            }
        });
        let mut sockets = SocketSet::new(vec![]);
        let tftp = TftpServer::new(&mut sockets, config.tftp_root.clone());
        log::info!(
            "virtual network: server {} gateway {} /{} tftp root {:?}",
            config.server_ip,
            config.gateway_ip,
            config.prefix_len,
            config.tftp_root
        );
        VirtualNetwork { cable, iface, sockets, tftp, config, next_idle_poll_us: 0 }
    }

    /// A frame the guest put on the wire (without FCS).
    pub fn guest_transmit(&mut self, frame: Vec<u8>) {
        log::debug!("guest -> net  {}", describe(&frame));
        if self.cable.from_guest.len() < MAX_QUEUED_FRAMES {
            self.cable.from_guest.push_back(frame);
        }
    }

    /// Next frame for the guest, without removing it (the guest may have no
    /// free receive buffer yet).
    pub fn peek_for_guest(&self) -> Option<&Vec<u8>> {
        self.cable.to_guest.front()
    }

    pub fn pop_for_guest(&mut self) -> Option<Vec<u8>> {
        let f = self.cable.to_guest.pop_front()?;
        log::debug!("net -> guest  {}", describe(&f));
        Some(f)
    }

    /// Is a poll useful now? Frames from the guest, a running TFTP
    /// transfer, or the periodic housekeeping.
    pub fn wants_poll(&self, now_us: u64) -> bool {
        !self.cable.from_guest.is_empty() || self.tftp.busy() || now_us >= self.next_idle_poll_us
    }

    /// Let the virtual host process what the guest sent and run its timers.
    /// `now_us` is the emulator's virtual time in microseconds.
    pub fn poll(&mut self, now_us: u64) {
        self.next_idle_poll_us = now_us + IDLE_POLL_US;
        let now = Instant::from_micros(now_us as i64);
        self.iface.poll(now, &mut self.cable, &mut self.sockets);
        if self.tftp.service(&mut self.sockets, now) {
            // the server queued replies: push them out right away
            self.iface.poll(now, &mut self.cable, &mut self.sockets);
        }
    }
}

/// What the router's Ethernet port is plugged into (`--net`, docs ch. 18).
pub enum Link {
    /// The built-in virtual host (ARP, ping, TFTP), no privileges needed.
    Virtual(Box<VirtualNetwork>),
    /// A TAP device or a UDP tunnel.
    Host(host::HostLink),
}

impl Link {
    pub fn guest_transmit(&mut self, frame: Vec<u8>) {
        match self {
            Link::Virtual(v) => v.guest_transmit(frame),
            Link::Host(h) => h.guest_transmit(frame),
        }
    }

    pub fn peek_for_guest(&self) -> Option<&Vec<u8>> {
        match self {
            Link::Virtual(v) => v.peek_for_guest(),
            Link::Host(h) => h.peek_for_guest(),
        }
    }

    pub fn pop_for_guest(&mut self) -> Option<Vec<u8>> {
        match self {
            Link::Virtual(v) => v.pop_for_guest(),
            Link::Host(h) => h.pop_for_guest(),
        }
    }

    /// Is a poll useful now? (Host links: always, it is just a `try_recv`.)
    pub fn wants_poll(&self, now_us: u64) -> bool {
        match self {
            Link::Virtual(v) => v.wants_poll(now_us),
            Link::Host(_) => true,
        }
    }

    pub fn poll(&mut self, now_us: u64) {
        match self {
            Link::Virtual(v) => v.poll(now_us),
            Link::Host(h) => h.poll(),
        }
    }
}

/// One-line human description of an Ethernet frame, for the debug log.
pub fn describe(frame: &[u8]) -> String {
    let Ok(eth) = EthernetFrame::new_checked(frame) else { return format!("runt frame ({} bytes)", frame.len()) };
    match eth.ethertype() {
        EthernetProtocol::Arp => match ArpPacket::new_checked(eth.payload()) {
            Ok(arp) => {
                let ip = |b: &[u8]| Ipv4Address::new(b[0], b[1], b[2], b[3]);
                format!(
                    "ARP op {} {} -> who-has/is {}",
                    u16::from(arp.operation()),
                    ip(arp.source_protocol_addr()),
                    ip(arp.target_protocol_addr())
                )
            }
            Err(_) => "bad ARP".into(),
        },
        EthernetProtocol::Ipv4 => match Ipv4Packet::new_checked(eth.payload()) {
            Ok(ip) => {
                let head = format!("IPv4 {} -> {}", ip.src_addr(), ip.dst_addr());
                match ip.next_header() {
                    IpProtocol::Udp => match UdpPacket::new_checked(ip.payload()) {
                        Ok(udp) => {
                            let p = udp.payload();
                            let tftp = match (p.first(), p.get(1)) {
                                (Some(0), Some(op @ 1..=6)) => {
                                    let name = ["", "RRQ", "WRQ", "DATA", "ACK", "ERROR", "OACK"][*op as usize];
                                    let blk = if p.len() >= 4 { u16::from_be_bytes([p[2], p[3]]) } else { 0 };
                                    format!(" TFTP {name} {blk} ({} bytes)", p.len())
                                }
                                _ => String::new(),
                            };
                            format!("{head} UDP {} -> {}{tftp}", udp.src_port(), udp.dst_port())
                        }
                        Err(_) => format!("{head} bad UDP"),
                    },
                    IpProtocol::Icmp => format!("{head} ICMP"),
                    other => format!("{head} proto {other}"),
                }
            }
            Err(_) => "bad IPv4".into(),
        },
        other => format!("ethertype {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Internet checksum (RFC 1071) for building test packets.
    fn csum(data: &[u8]) -> u16 {
        let mut sum: u32 = data.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !(sum as u16)
    }

    /// A ping from the router (10.0.2.15) to the virtual host gets an echo
    /// reply. smoltcp only answers pings with its `auto-icmp-echo-reply`
    /// feature, which our minimal feature list had left out.
    #[test]
    fn virtual_host_answers_ping() {
        let cfg = NetConfig {
            server_ip: "10.0.2.2".parse().unwrap(),
            gateway_ip: "10.0.2.1".parse().unwrap(),
            prefix_len: 24,
            tftp_root: None,
        };
        let mut net = VirtualNetwork::new(cfg);
        let guest_mac = [0x02, 0x17, 0, 0, 0, 1];
        let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1];
        icmp.extend((0..72).map(|i| i as u8));
        let c = csum(&icmp);
        icmp[2..4].copy_from_slice(&c.to_be_bytes());
        let mut ip = vec![0x45, 0, 0, 0, 0, 1, 0, 0, 255, 1, 0, 0, 10, 0, 2, 15, 10, 0, 2, 2];
        let total = (20 + icmp.len()) as u16;
        ip[2..4].copy_from_slice(&total.to_be_bytes());
        let c = csum(&ip);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        let mut frame = HOST_MAC.to_vec();
        frame.extend(guest_mac);
        frame.extend([0x08, 0x00]);
        frame.extend(ip);
        frame.extend(icmp);

        // ARP first, so smoltcp knows where to send the reply (as with IOS)
        let mut arp = vec![0xFF; 6];
        arp.extend(guest_mac);
        arp.extend([0x08, 0x06, 0, 1, 8, 0, 6, 4, 0, 1]);
        arp.extend(guest_mac);
        arp.extend([10, 0, 2, 15, 0, 0, 0, 0, 0, 0, 10, 0, 2, 2]);
        net.guest_transmit(arp);
        net.poll(1_000);
        while net.pop_for_guest().is_some() {}
        net.guest_transmit(frame);
        net.poll(2_000);
        let replies: Vec<String> = std::iter::from_fn(|| net.pop_for_guest()).map(|f| describe(&f)).collect();
        assert!(replies.iter().any(|d| d.contains("10.0.2.2 -> 10.0.2.15 ICMP")), "{replies:?}");
    }
}
