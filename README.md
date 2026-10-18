# c1700emu — a Cisco 1700 router emulator

`c1700emu` emulates a Cisco 1700-series router (Motorola MPC860T PowerPC)
well enough to run the **real boot ROM** (ROMMON 12.2(1r)XE1) and **IOS
12.4**: console, flash, NVRAM, Ethernet (to a virtual host, a Linux TAP
device or another emulator), WIC serial cards, snapshots, a GDB stub, and a
JIT that runs IOS about 20 times faster than the interpreter.

```
System Bootstrap, Version 12.2(1r)XE1, RELEASE SOFTWARE (fc1)
...
Cisco 1760 (MPC860T) processor (revision 0x00) with 114688K/16384K bytes of memory.
1 FastEthernet interface
32768K bytes of processor board System flash (Read/Write)

Router>
```

It is written in Rust, from scratch: its own PowerPC interpreter and
Cranelift JIT, the MPC860's on-chip devices and the C1700 board. How every
piece was found and built is told step by step in [`docs/`](docs/README.md).

## Contents

1. [Requirements](#1-requirements)
2. [Get the code and build it](#2-get-the-code-and-build-it)
3. [The files you need: boot ROM and IOS](#3-the-files-you-need-boot-rom-and-ios)
4. [First run: ROMMON](#4-first-run-rommon)
5. [Booting IOS](#5-booting-ios)
6. [Everyday use: snapshots](#6-everyday-use-snapshots)
7. [The console](#7-the-console)
8. [Networking](#8-networking)
9. [Serial WAN cards (WIC-1T, WIC-2T)](#9-serial-wan-cards-wic-1t-wic-2t)
10. [Debugging and profiling](#10-debugging-and-profiling)
11. [Tests and benchmark](#11-tests-and-benchmark)
12. [Command-line reference](#12-command-line-reference)
13. [Repository layout](#13-repository-layout)

## 1. Requirements

* **Linux** on x86-64 (developed and tested on Fedora; the TAP network
  support is Linux-only, other systems are untested).
* **Rust**, a recent stable toolchain (tested with 1.99; the code uses Rust
  2024 edition features). The easiest way is [rustup](https://rustup.rs):

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

  Distribution packages work if they are recent enough (Fedora:
  `sudo dnf install rust cargo`).
* **A C compiler** (`gcc` or `clang`): one dependency, the TAP device
  crate, builds a small C helper. Fedora: `sudo dnf install gcc`;
  Debian/Ubuntu: `sudo apt install build-essential`.
* **Python 3**, only for the helper scripts in `tools/` (scripted IOS
  sessions, tests).
* Optional: `telnet` (a second console), `gdb-multiarch` or a PowerPC GDB
  (debugging the guest), `perf` (profiling).

## 2. Get the code and build it

```bash
git clone git@github.com:IridiumXOR/c1700emu.git
cd c1700emu
cd emu && cargo build --release && cd ..
```

The binary is `emu/target/release/c1700emu`. The first build downloads the
Rust dependencies and takes a minute or two. Run the emulator **from the
repository root**: the default boot ROM path (`roms/...`) is relative to it.
`c1700emu --help` lists every option.

## 3. The files you need: boot ROM and IOS

Cisco's software is not part of this repository. You need:

* **The boot ROM**: a 512 KB dump of the C1700's boot EPROM (an AT27C040)
  containing System Bootstrap 12.2(1r)XE1. By default the emulator looks
  for `roms/CISCO_bootrom_AT27C040.bin`; `--rom FILE` points elsewhere.
* **An IOS image** for the 1700 series, to go further than ROMMON. The one
  tested end to end is `c1700-adventerprisek9-mz.124-8.bin` (IOS 12.4(8),
  21 MB), which needs a voice-capable chassis (`--model 1760`, or 1750/1751)
  and 128 MB of RAM. Other C1700 images should work but are untested.

(The author keeps these in a private companion repository, cloned as
`roms/`: `git clone git@github.com:IridiumXOR/c1700emu-roms.git roms`.)

## 4. First run: ROMMON

```bash
emu/target/release/c1700emu --confreg 0x2100
```

`--confreg 0x2100` sets the configuration register's boot field to 0:
stay in ROMMON. On a fresh NVRAM the ROM warns once that its NVRAM area is
corrupt (it then writes defaults); that is normal.

```
System Bootstrap, Version 12.2(1r)XE1, RELEASE SOFTWARE (fc1)
TAC Support: http://www.cisco.com/cgi-bin/ibld/view.pl?i=support
Copyright (c) 2001 by cisco Systems, Inc.
Warning: monitor nvram area is corrupt ... using default values
C1700 platform with 32768 Kbytes of main memory

rommon 1 > dev
```

Try `help`, `meminfo`, `dev`, `dir flash:`, `confreg`. Quit with
**`Ctrl-]` then `q`**.

The emulator keeps its state in files in the current directory:
`c1700-nvram.bin` (NVRAM: configuration register, IOS startup config) and
`c1700-flash.bin` (the internal flash). Both are saved automatically and
reloaded at the next start (`--nvram FILE`, `--flash FILE` to choose
others).

## 5. Booting IOS

Put the IOS image into the emulated flash and let ROMMON boot it:

```bash
emu/target/release/c1700emu --model 1760 --ram 128 --confreg 0x2102 \
    --flash-file roms/c1700-adventerprisek9-mz.124-8.bin --cpu jit
```

* `--model 1760 --ram 128`: a Cisco 1760 with 128 MB, what this image needs.
* `--confreg 0x2102`: the usual IOS setting, boot the first image in flash.
* `--flash-file IMAGE`: builds a flash file system containing the image and
  writes it to `c1700-flash.bin`. You only need it once: next time the
  flash file already holds it.
* `--cpu jit`: translate hot code to host machine code (about 20 times the
  interpreter's speed). The interpreter (`--cpu interp`, the default) is the
  reference implementation.

ROMMON checks and decompresses the image, IOS boots and asks:

```
Would you like to enter the initial configuration dialog? [yes/no]: no
Press RETURN to get started!
Router>
```

Answer `no`, press RETURN, and you are at the IOS prompt (`enable`,
`show version`, `show interfaces`, `configure terminal`...). `write memory`
saves the configuration into the emulated NVRAM.

**Boot time.** By default the emulated router keeps the wall clock's time
(idle detection skips IOS's idle loop, chapter 15 of the docs), so it boots
in real time: about 2.5 minutes to the prompt. `--idle off` lets virtual
time run as fast as the host can go: about **20 seconds** with the JIT. Use
it for booting, but not for interactive or networked work, where the
router's clock should match the real one. Snapshots (next section) give
you the best of both.

## 6. Everyday use: snapshots

Boot once, then save the whole machine at the prompt with **`Ctrl-]` `s`**:

```
[c1700emu] snapshot saved to c1700.snap (70 MB, 0.2 s); restore with --restore
```

Next time start from it, and IOS is at the prompt immediately:

```bash
emu/target/release/c1700emu --model 1760 --ram 128 --cpu jit --restore c1700.snap
```

Restore with the same `--model`, `--ram`, boot ROM and cards (`--wic0`...)
as when the snapshot was taken (the RAM and flash sizes and the ROM are
checked, with a message on a mismatch). `--snapshot FILE` chooses where `Ctrl-] s` writes. Snapshots are
tied to the emulator version that wrote them (a new snapshot format is
refused with a message: take a new one).

## 7. The console

The console is the terminal you started the emulator in. Keys after
**`Ctrl-]`**:

| keys | action |
|---|---|
| `Ctrl-]` `q` | quit (NVRAM and flash are saved) |
| `Ctrl-]` `b` | send a BREAK on the console line (the key Cisco's password recovery uses during a boot) |
| `Ctrl-]` `s` | save a snapshot |
| `Ctrl-]` `d` | write an ELF core dump of the machine (for GDB) |
| `Ctrl-]` `p` | reset the profiling window (`--profile`) |
| `Ctrl-]` `Ctrl-]` | send a literal `Ctrl-]` |

Telnet consoles:

```bash
c1700emu ... --tcp 2323                 # console also on telnet 127.0.0.1 2323
c1700emu ... --tcp 2323 --no-stdio      # only on telnet (e.g. in the background)
c1700emu ... --aux-tcp 2324             # the router's AUX port (IOS runs a second exec there)
```

## 8. Networking

The router's FastEthernet0/0 can be plugged into one of these (`--net`):

**The built-in virtual host** (default, `--net virtual`): answers ping and
ARP at 10.0.2.2 and serves files over TFTP from `--tftp-root DIR`.

```
Router(config)# interface FastEthernet0/0
Router(config-if)# ip address 10.0.2.15 255.255.255.0
Router(config-if)# no shutdown
Router# ping 10.0.2.2
!!!!!
```

**A Linux TAP device**, to reach the host and, through a bridge or NAT, the
real network. Create it once (owned by your user, so the emulator needs no
root), then plug the router into it:

```bash
sudo ip tuntap add dev tap0 mode tap user $USER
sudo ip addr add 192.168.100.1/24 dev tap0
sudo ip link set tap0 up

c1700emu ... --net tap:tap0
```

Give the router 192.168.100.2 and it pings the host and back. Bridging and
NAT setups are in [`docs/18-real-networks.md`](docs/18-real-networks.md).

**Another emulator**, through a UDP tunnel (one Ethernet frame per
datagram, as Dynamips/GNS3 do). Give each router its own MAC address:

```bash
c1700emu ... --net udp:20001:127.0.0.1:20002 --mac 02:17:00:00:00:01   # router A
c1700emu ... --net udp:20002:127.0.0.1:20001 --mac 02:17:00:00:00:02   # router B
```

`--net none` leaves the port unplugged.

**Network boot from ROMMON** (`tftpdnld`): `--tftp-root DIR` serves `DIR`
on the virtual network; `tools/mkimage.py` builds small test images. See
[`docs/08-network-and-flash.md`](docs/08-network-and-flash.md).

## 9. Serial WAN cards (WIC-1T, WIC-2T)

Fit a serial card in a WIC slot and cable its ports to another emulator
(one HDLC frame per UDP datagram):

```bash
# router A: WIC-1T in slot 0, its port cabled to router B
c1700emu ... --wic0 wic-1t --serial0 udp:31000:127.0.0.1:31001
# router B
c1700emu ... --wic0 wic-1t --serial0 udp:31001:127.0.0.1:31000
```

IOS sees the card (`show diag`) and a `Serial0/0` interface; configure an
address on both ends, `no shutdown`, and the line protocol comes up on the
HDLC keepalives:

```
Router# show interfaces Serial0/0
Serial0/0 is up, line protocol is up
     DCD=up  DSR=up  DTR=up  RTS=up  CTS=up
```

* `--wic0`/`--wic1`: `wic-1t` (one port) or `wic-2t` (two ports). IOS
  numbers the ports Serial0/0, 0/1... in slot order.
* `--serial0`, `--serial0-1`: the first and second port of the card in slot
  0; `--serial1`, `--serial1-1`: slot 1. Without one, that port has no
  cable ("No serial cable attached").
* The cable is a V.35 DTE cable; append `,dce` for the DCE end
  (`--serial0 udp:31000:127.0.0.1:31001,dce`), which takes `clock rate`.
* Two identically configured routers started together run cycle for cycle
  alike, and IOS then reports the line as `(looped)`: give them different
  keepalive periods (e.g. `keepalive 7` on one side). Details in
  [`docs/23-wic-1t-serial.md`](docs/23-wic-1t-serial.md).

## 10. Debugging and profiling

* **GDB**: `--gdb 1234` waits for a debugger before running anything:

  ```
  $ gdb                              # gdb-multiarch on Debian/Ubuntu
  (gdb) set architecture powerpc:common
  (gdb) set endian big
  (gdb) target remote :1234
  (gdb) break *0xfff060d8            # breakpoints work in ROM too
  (gdb) continue
  ```

* **Core dumps**: `Ctrl-] d` writes `c1700-core.elf` (memory and registers):

  ```bash
  gdb -q -ex "set architecture powerpc:common" -ex "core-file c1700-core.elf"
  ```

  (`gdb-multiarch` on Debian/Ubuntu; [`docs/20-snapshots-and-core-dumps.md`](docs/20-snapshots-and-core-dumps.md)
  shows a session.)
* **What the guest touched**: `--stats` prints, at exit, the exceptions
  taken, every on-chip register and board device accessed, and the JIT's
  statistics.
* **Logs**: `--log FILE --log-level info` (or `debug`, with targets such as
  `--log-level warn,net=debug,scc=debug`); each line carries wall-clock
  time.
* **Watching memory**: `--watch 07040000:4000` logs every access to that
  physical range with the PC that made it.
* **Profiles**: `--profile` (hot ROM addresses, and PC samples of the whole
  run with `--profile-dump FILE`); with the JIT,
  `C1700_PERF_MAP=1 perf record -e cycles:u -- c1700emu --cpu jit ...` lets
  `perf report` name translated code by guest address.
* `--cpu jit-check` runs the JIT and verifies every translated block
  against the interpreter (slow; for finding JIT bugs).

## 11. Tests and benchmark

```bash
cd emu && cargo test --release
```

runs the unit tests, the CPU tests, the JIT tests and the board tests. A
differential test runs random instruction sequences under QEMU's user-mode
PowerPC emulator and compares; it needs `qemu-ppc-static`, `llvm-mc` and
`ld.lld`, and is skipped (with a message) without them. Tests that need the boot ROM skip themselves (with a
message) when `roms/` is missing.

End-to-end scripts (they need the boot ROM and the IOS image, and run full
IOS sessions):

| script | what it checks |
|---|---|
| `tools/tests/serial_test.sh` | two routers with WIC-1Ts: line protocol up, pings both ways |
| `tools/tests/serial_2t_test.sh` | two routers with WIC-2Ts cabled twice, with DCE ends |
| `tools/tests/tap_test.sh SNAPSHOT` | a TAP device in an unprivileged network namespace: router ↔ host pings |
| `tools/tests/warp_test.py SNAPSHOT` | `--idle warp`: idle time skipped, console still answering |
| `tools/tests/exec_timeout_test.py SNAPSHOT` | the console comes back after an IOS logout |

`tools/ios_session.py` drives an IOS boot and a list of commands
unattended; the scripts above show how to use it.

`tools/bench.sh [--cpu jit]` measures the CPU engine (power-on to 3 billion
executed instructions with IOS in flash): about 36 million instructions per
second for the interpreter and 700 for the JIT on a recent desktop CPU.

## 12. Command-line reference

The most used options (all of them: `c1700emu --help`, and
[`docs/07-running-and-debugging.md`](docs/07-running-and-debugging.md)):

| option | default | meaning |
|---|---|---|
| `--rom FILE` | `roms/CISCO_bootrom_AT27C040.bin` | boot ROM image |
| `--model M` | `1720` | 1720, 1721, 1750, 1751 or 1760 |
| `--ram MB` | `32` | main memory (up to 256) |
| `--nvram FILE`, `--flash FILE` | `c1700-nvram.bin`, `c1700-flash.bin` | persistent NVRAM and flash |
| `--flash-file IMAGE` | | put a file (an IOS image) into the flash file system |
| `--confreg 0xNNNN` | | write the configuration register (0x2100 ROMMON, 0x2102 boot IOS) |
| `--cpu interp\|jit\|jit-check` | `interp` | CPU engine |
| `--idle realtime\|warp\|off` | `realtime` | idle handling: keep the wall clock, skip idle time freely, or none |
| `--net virtual\|none\|tap:NAME\|udp:LP:HOST:RP` | `virtual` | what the Ethernet port is plugged into |
| `--mac XX:XX:XX:XX:XX:XX` | `02:17:00:00:00:01` | Ethernet MAC address (in the NVRAM cookie) |
| `--tftp-root DIR` | | TFTP server on the virtual network |
| `--wic0`, `--wic1 wic-1t\|wic-2t` | | serial cards in the WIC slots |
| `--serial0` ... `--serial1-1 udp:LP:HOST:RP[,dce]` | | cable a serial port |
| `--tcp PORT`, `--aux-tcp PORT`, `--no-stdio` | | telnet consoles |
| `--restore FILE`, `--snapshot FILE`, `--core FILE` | `c1700.snap`, `c1700-core.elf` | snapshots and core dumps |
| `--gdb PORT` | | wait for GDB |
| `--stats`, `--log FILE`, `--log-level L` | `warn` | diagnostics |
| `-n N` | | stop after N instructions |

## 13. Repository layout

| path | contents |
|---|---|
| `emu/ppc8xx/` | the PowerPC (MPC8xx) core: interpreter, MMU, JIT (Cranelift) |
| `emu/c1700/` | the board and the `c1700emu` program: MPC860 devices, CPLD, flash, NVRAM, UART, FEC, SCC, WIC cards, networking, snapshots |
| `docs/` | 23 chapters on how it was all found and built ([index](docs/README.md), [summary](docs/00-summary.md)) |
| `tools/` | helper scripts: IOS sessions, image tools, IDA annotation scripts, tests, benchmark |
| `TODO.md` | the work list |
| `*.pdf` | reference manuals (MPC860 user's manual, Cisco documents) |
| `roms/` | not in this repository: your boot ROM and IOS images |
