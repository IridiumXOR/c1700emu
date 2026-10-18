# 18. Real networks: TAP devices and UDP tunnels

Until now the router's FastEthernet port was plugged into a built-in
virtual host (chapter 8): one machine at 10.0.2.2 that answers ARP and
ping and serves TFTP. That needs no privileges and is still the default,
but the router can't reach anything else. This chapter adds two real
attachments:

```
c1700emu --net virtual                       # the built-in host (default)
c1700emu --net none                          # cable unplugged (= --no-net)
c1700emu --net tap:tap0                      # a Linux TAP device
c1700emu --net udp:20001:127.0.0.1:20002     # frames in UDP datagrams
c1700emu --mac 02:17:00:00:00:02 ...         # each router its own MAC
```

## 18.1 How the port is wired

The FEC model (chapter 8) hands every transmitted frame (without FCS) to
whatever is plugged in, and takes received frames from it when the guest
has a free receive buffer. That interface is now an enum, `net::Link`:

| link | transmit | receive |
|---|---|---|
| `Virtual` | into smoltcp's frame queue | smoltcp's replies, at the next poll |
| `Host` (TAP, UDP) | one `send` system call | a background thread blocks on the device and queues frames; the emulation loop only does a cheap `try_recv` |

The receive queue is bounded (256 frames): a guest that stops receiving
can't make the emulator buffer forever, exactly like a real NIC dropping
frames when its FIFO is full. Short frames are padded to 60 bytes and get
their FCS appended by the FEC model, so frames from a TAP device or a UDP
peer look like frames off a real cable.

**Timing.** Real networks run on wall-clock time, the emulator on virtual
time. With idle detection in realtime mode (the default, chapter 15) the
two stay in step. Frames that arrive while the emulator is skipping idle
time are delivered at the next device sync, a few milliseconds later at
most.

## 18.2 TAP: the router on the host's network

A TAP device is a virtual Ethernet interface: what the router sends
appears on it, what the host sends to it reaches the router. Creating one
needs root once; after that the emulator opens it as a normal user:

```bash
sudo ip tuntap add dev tap0 mode tap user $USER
sudo ip addr add 192.168.100.1/24 dev tap0
sudo ip link set tap0 up
emu/target/release/c1700emu --net tap:tap0 ...
```

and in IOS:

```
Router(config)# interface FastEthernet0/0
Router(config-if)# ip address 192.168.100.2 255.255.255.0
Router(config-if)# no shutdown
Router# ping 192.168.100.1
```

From the host, `ping 192.168.100.2` and `telnet 192.168.100.2` reach the
router (after configuring `line vty` and a password in IOS). To put the
router on the real LAN, either bridge the TAP device with the physical
interface (`ip link add br0 type bridge`, enslave both, move the host's
address to `br0`), or route/NAT the 192.168.100.0/24 network on the host.

The crate is `tun-tap` (without its tokio feature): it opens the existing
device with `TUNSETIFF` and gives blocking `send`/`recv`.

## 18.3 UDP tunnels: cabling emulators together

Dynamips and GNS3 connect emulated routers with "UDP NIOs": every
Ethernet frame travels as one UDP datagram. `--net udp:LPORT:RHOST:RPORT`
does the same, which needs no privileges at all. Two routers on one
machine:

```bash
c1700emu --nvram a.nv --flash a.fl --mac 02:17:00:00:00:01 --net udp:20001:127.0.0.1:20002 ...
c1700emu --nvram b.nv --flash b.fl --mac 02:17:00:00:00:02 --net udp:20002:127.0.0.1:20001 ...
```

`--mac` writes the MAC address into the NVRAM cookie (chapter 4), so each
router has its own. A connected UDP socket reports "connection refused"
while the other end isn't listening yet (an ICMP port-unreachable comes
back); the receiver thread just waits on.

Tried with two IOS 12.4(8) routers (JIT, idle detection on), A at
10.1.1.1 and B at 10.1.1.2, both booting in parallel:

```
A# ping 10.1.1.2
Sending 5, 100-byte ICMP Echos to 10.1.1.2, timeout is 2 seconds:
.!!!!
Success rate is 80 percent (4/5), round-trip min/avg/max = 12/20/24 ms

B# show arp
Internet  10.1.1.2                -   0217.0000.0002  ARPA   FastEthernet0/0
Internet  10.1.1.1                0   0217.0000.0001  ARPA   FastEthernet0/0
B# show interfaces FastEthernet0/0
     26 packets input, 7677 bytes
     31 packets output, 7977 bytes, 0 underruns
```

The first echo is lost to ARP, as between real routers; the extra packets
are the routers' own keepalives and CDP announcements.

## 18.4 Tests

* `net::host::tests::udp_tunnel_carries_frames_both_ways`: two UDP link
  ends on localhost exchange a 60-byte and a 42-byte frame.
* The two-router session above, driven by `tools/ios_session.py`.
* TAP needs root to create the device, so it is tested by hand with the
  commands of 18.2.
