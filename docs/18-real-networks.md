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

A **TAP** device is a virtual Ethernet interface in the Linux kernel: a
program (here the emulator) holds one end, the host's network stack sees
the other end as an interface like `eth0`. Every frame the router sends
appears on `tap0`, and every frame the host sends out of `tap0` reaches
the router's FastEthernet0/0. (A **TUN** device is the same idea one layer
up, carrying IP packets instead of Ethernet frames; the router needs
Ethernet, so always `mode tap`.)

### Prerequisites

* The `tun` kernel module (loaded automatically on most distributions;
  check with `ls -l /dev/net/tun`, load with `sudo modprobe tun`).
* Root, **once**, to create the device. Created with `user $USER`, the
  device belongs to you, and the emulator then opens it without any
  privileges.

### Step 1: create the device

```bash
sudo ip tuntap add dev tap0 mode tap user $USER   # create, owned by you
sudo ip link set tap0 up                          # bring the host side up
ip tuntap show                                    # check: "tap0: tap persist user 1000"
```

The device is *persistent*: it stays until it is deleted or the machine
reboots, whether or not the emulator is running. Start the emulator with
`--net tap:tap0`; IOS's interface comes up when you `no shutdown` it.

### Step 2: choose how the router reaches the world

**(a) Host only**: the host and the router share a small private network.
Enough to ping, telnet into the router, or copy files over TFTP from a
server on the host.

```bash
sudo ip addr add 192.168.100.1/24 dev tap0
```

```
Router(config)# interface FastEthernet0/0
Router(config-if)# ip address 192.168.100.2 255.255.255.0
Router(config-if)# no shutdown
Router# ping 192.168.100.1
```

**(b) Routed with NAT**: the router also reaches the internet through
the host (like a home router behind the PC). On top of (a):

```bash
sudo sysctl -w net.ipv4.ip_forward=1
# replace eth0 with the host's uplink interface (see `ip route show default`)
sudo nft add table ip c1700
sudo nft add chain ip c1700 post '{ type nat hook postrouting priority 100 ; }'
sudo nft add rule ip c1700 post ip saddr 192.168.100.0/24 oifname "eth0" masquerade
```

```
Router(config)# ip route 0.0.0.0 0.0.0.0 192.168.100.1
Router(config)# ip name-server 8.8.8.8
Router# ping 8.8.8.8
```

(With iptables instead of nftables: `sudo iptables -t nat -A POSTROUTING -s
192.168.100.0/24 -o eth0 -j MASQUERADE`. A host firewall such as firewalld
may also need tap0 in a trusted zone: `sudo firewall-cmd --zone=trusted
--add-interface=tap0`.)

**(c) Bridged into the LAN**: the router becomes one more machine on the
physical network, gets an address from the same DHCP server or a static
one, and other PCs reach it directly. The host's wired interface and tap0
go into a bridge, and the host's address moves to the bridge:

```bash
sudo ip link add br0 type bridge
sudo ip link set eth0 master br0      # the wired uplink (Wi-Fi can't be bridged)
sudo ip link set tap0 master br0
sudo ip link set br0 up
sudo dhclient br0                     # or move the host's static address to br0
```

Do this from a local console, not over SSH through `eth0`: the host loses
its address on `eth0` for a moment. With NetworkManager it is easier to
create the bridge there (`nmcli connection add type bridge ifname br0`,
then add `eth0` as a bridge port) and only enslave tap0 by hand. In IOS,
give FastEthernet0/0 a free address of the LAN, or `ip address dhcp`.
Each router on the LAN needs its own MAC (`--mac`).

### Using it

```bash
emu/target/release/c1700emu --net tap:tap0 --mac 02:17:00:00:00:01 ...
```

From the host (setup a or b), `ping 192.168.100.2` reaches the router, and
`telnet 192.168.100.2` too once IOS has `line vty 0 4` / `password ...` /
`login`. `sudo tcpdump -ni tap0` shows every frame the router exchanges,
which is the quickest way to see what is going on.

### Cleaning up

```bash
sudo ip link del tap0                       # or: sudo ip tuntap del dev tap0 mode tap
sudo nft delete table ip c1700              # if you used (b)
sudo ip link set eth0 nomaster; sudo ip link del br0   # if you used (c)
```

### When it doesn't work

| symptom | cause |
|---|---|
| `cannot open TAP device tap0: Operation not permitted` | the device doesn't exist (creating one needs root), or it belongs to another user: create it with `user $USER` |
| `... No such file or directory` | `/dev/net/tun` missing: `sudo modprobe tun` |
| the router's interface is up but nothing answers | host side down (`sudo ip link set tap0 up`), no address on tap0 (setup a), or a host firewall dropping tap0 traffic |
| ping works from the host but not to the internet | NAT or forwarding missing (setup b), or no default route in IOS |
| two routers on one bridge interfere | they have the same MAC: give each its own `--mac` |

The crate is `tun-tap` (without its tokio feature): it opens the existing
device with the `TUNSETIFF` ioctl and gives blocking `send`/`recv`.

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
* `tools/tests/tap_test.sh SNAPSHOT`: the router on a TAP device, end to
  end, **without sudo**. An unprivileged user + network namespace
  (`unshare -r -n`) makes the script root inside a private network stack,
  where it may create `tap0`; the emulator runs in there too, restored
  from a snapshot of IOS at the prompt (chapter 20). IOS pings the host
  side of the TAP device and the host pings the router
  (`tools/ios_session.py`'s `host` directive):

  ```
  Router# ping 192.168.100.1
  Success rate is 80 percent (4/5), round-trip min/avg/max = 1/1/1 ms
  [host] $ ping -c 3 -W 2 192.168.100.2
  64 bytes from 192.168.100.2: icmp_seq=1 ttl=255 time=0.685 ms
  3 packets transmitted, 3 received, 0% packet loss
  Router# show arp
  Internet  192.168.100.1           0   86c2.dadf.64a7  ARPA   FastEthernet0/0
  ```

  The same trick is handy for experiments: `unshare -r -n` gives a
  throw-away network with TAP devices, bridges and NAT, no root needed.
