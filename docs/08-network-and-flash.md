# 8. Network boot (`tftpdnld`) and flash persistence

Goal: make ROMMON's `tftpdnld` work — download an image over Ethernet, run
it from RAM or write it to flash — and keep the flash content across runs.

```text
  ROMMON tftpdnld ─► fec_transmit ─► TxBD ring in RAM ─┐            ┌─ TFTP server (tftp.rs)
                                                       ▼            │   files from --tftp-root
                        board/fec_dma.rs  (the FEC's DMA engine)    │
                                                       ▲            │
  ROMMON fec_receive_poll ◄─ RxBD ring in RAM ◄────────┘  net/mod.rs: smoltcp Interface
                                                          (ARP, ping, UDP) on a "virtual cable"
```

## 8.1 What ROMMON's `tftpdnld` does (reversed)

Found through the monitor's command table (entry `tftpdnld` →
`cmd_tftpdnld` at 0xFFF1EF1C) and decompiled with headless IDA
(`tools/ida/decompile.py`, chapter 2):

1. Reads the variables **IP_ADDRESS, IP_SUBNET_MASK, DEFAULT_GATEWAY,
   TFTP_SERVER, TFTP_FILE** (and TFTP_CHECKSUM, default on) and prints them.
2. Without `-r` it warns *"Invoke this command for disaster recovery only.
   WARNING: all existing data in all partitions on flash will be lost!"* and
   asks for confirmation.
3. `eth_init_fec_based` (0xFFF1AEE0 — the name comes from its own debug
   strings) resets the FEC, programs the MAC address from the cookie,
   `R_BUFF_SIZE = 1520`, the two descriptor rings, and **IMASK = 0**: the
   driver polls, no Ethernet interrupts.
4. ARP for the server, TFTP read request, `!` every 10 blocks, then
   *"File reception completed."*
5. `tftp_image_received` (0xFFF1E4A0) validates the image (8.4), then either
   loads and runs it (`-r`) or erases the flash and copies it there.

## 8.2 The FEC's buffer descriptors (MPC860UM §43.4.3)

The FEC is a bus master: it reads and writes *buffer descriptors* (BDs) in
RAM. A BD is 8 bytes:

| Offset | TxBD | RxBD |
|--------|------|------|
| +0 status | R 0x8000 (ready), W 0x2000 (wrap), L 0x0800 (last), TC 0x0400 (append CRC) | E 0x8000 (empty), W, L, BC 0x0080, MC 0x0040, errors |
| +2 length | bytes to send | bytes stored (whole frame + 4-byte CRC when L) |
| +4 buffer | pointer | pointer (16-byte aligned) |

Ownership protocol:

* **transmit**: software fills a BD, sets R, writes X_DES_ACTIVE; the FEC
  sends the buffer and clears R.
* **receive**: software arms BDs with E set and writes R_DES_ACTIVE; the FEC
  fills one per frame (more if the frame is longer than R_BUFF_SIZE), clears
  E, sets L and the length.
* The BD with W is the last of its ring; the next one is at
  R_DES_START/X_DES_START.

The detail that matters: ROMMON's `fec_transmit` writes X_DES_ACTIVE **only
if it reads back 0**. The manual says the bit is set by any write and cleared
by the FEC when it finds a BD it doesn't own. A model that kept returning
"active" would send exactly one frame and then never again. The register
part lives in `mpc860/fec.rs`; the ring walking needs RAM access, so it lives
on the board in `board/fec_dma.rs` and runs every device sync (every 256
instructions). Received frames are padded to the Ethernet minimum (60 bytes)
and get a real CRC-32 appended (`crc32fast`), like the hardware stores it.

## 8.3 The virtual network

Real networking (a TAP device) needs root, so the emulator brings its own
network, the same idea as QEMU's "user networking":

* `net/mod.rs`: a **virtual cable** (two frame queues) implementing smoltcp's
  `phy::Device`; a smoltcp `Interface` owns two addresses, **10.0.2.2** (the
  server) and **10.0.2.1** (to use as DEFAULT_GATEWAY), answers ARP and ping.
  *Correction:* ping did **not** work until IOS tried it. smoltcp only
  answers echo requests with its `auto-icmp-echo-reply` cargo feature, which
  our minimal feature list (`default-features = false`) had left out.
  ROMMON only uses ARP and TFTP, so nothing noticed. The unit test
  `net::tests::virtual_host_answers_ping` now covers it.
* `net/tftp.rs`: a read-only **TFTP server** (RFC 1350) on smoltcp UDP
  sockets: RRQ on port 69 → a new port (transfer ID) per transfer → DATA
  blocks of 512 bytes, each sent after the previous ACK, retransmission after
  1 s, short block = end. Packet encoding uses the `tftp-packet` crate; no
  existing TFTP *server* crate can run on top of smoltcp, so the session logic
  is ours. Finished transfers close in two steps so the last packet (or an
  ERROR) leaves before the socket is removed.
* Time is **virtual** (CPU cycles at 48 MHz) on both sides, so timeouts agree
  regardless of host speed.
* `RUST_LOG=c1700emu::net=debug` prints every frame decoded on one line, e.g.

```text
guest -> net  ARP op 1 10.0.2.15 -> who-has/is 10.0.2.2
net -> guest  ARP op 2 10.0.2.2 -> who-has/is 10.0.2.15
guest -> net  IPv4 10.0.2.15 -> 10.0.2.2 UDP 28118 -> 69 TFTP RRQ ...
net -> guest  IPv4 10.0.2.2 -> 10.0.2.15 UDP 50000 -> 28118 TFTP DATA 1 (224 bytes)
guest -> net  IPv4 10.0.2.15 -> 10.0.2.2 UDP 28118 -> 50000 TFTP ACK 1 (4 bytes)
```

## 8.4 What a bootable C1700 image must look like

`image_header_check` (0xFFF18878) accepts a Unix-`compress` file (magic
0x1F9D) or an ELF with:

| ELF field | required | note |
|-----------|----------|------|
| e_ident | `\x7fELF`, 32-bit, big-endian | |
| e_type | 2 (executable) | |
| **e_machine** | **0x33** | not EM_PPC! It must equal `g_platform_code` (0x80006464), which `rom_init_main` sets to the cookie's processor byte. `readelf` calls 51 "Stanford MIPS-X" — Cisco reused the field as a platform ID, which is how ROMMON rejects images for other router families ("Wrong target platform") |
| e_ehsize / e_phentsize / e_shentsize | 52 / 32 / 40 | |
| e_phnum | 1 | |
| e_shoff | 84 | section headers right after the program header; then the file size must be `84 + 40·e_shnum + Σ sh_size` |

With TFTP_CHECKSUM on (default), `image_checksum_bad` (0xFFF1DC90) reads the
**last section**, which must start with the classic Cisco
self-decompressing-image trailer:

```text
+0  0xFEEDFACE   +4 uncompressed size   +8 compressed size
+12 compressed checksum   +16 uncompressed checksum   +20 payload...
checksum = 32-bit sum of words with end-around carry over "compressed size" bytes
```

The loader then copies every **section** to its `sh_addr` (the program
header is ignored) and jumps to `e_entry` — real IOS images are a small
loader section plus a last section with the compressed IOS behind that
trailer.

`tools/mkimage.py` builds such an image from 16 PowerPC instructions that
print a message on the 16550 (position independent, assembled with
`llvm-mc`, loaded at 0x80008000 like IOS).

## 8.5 Flash: what gets written, and persistence

After *"Copying file hello.bin to flash."* ROMMON erases every sector
(*"Erasing flash at 0x60000000"*, 128 KB steps = two 64 KB sectors in
parallel) and programs a Cisco flash file:

```text
bad00b1e 00000138 8f5d fffffff3 "hello.bin\0..." 7f454c46...
magic    length   cksum flags   name (pad)       the image
```

`dir flash:` lists it, and with confreg 0x2102 ROMMON boots it.

The flash model (now an Intel 28F128J3 x16 by default; the original 2 ×
Am29F016 with `--flash-size 4`; chapters 4 and 11) has a backing file: `--flash FILE` (default `c1700-flash.bin`) is loaded at start and
written back every 2 s while dirty and on exit — so a **new emulator process
autoboots the image ROMMON wrote** in a previous session. To skip ROMMON's
write entirely, `--flash-file` builds the flash file system directly
(chapter 11).

## 8.6 Two lessons from making it fast and correct

**The PIT was 11× too slow.** The first `tftpdnld` worked but sat silent for
tens of seconds after the last block. The profiler showed ROMMON looping in
its receive poll: after the final ACK it "dallies" (RFC 1350) and measures
that wait in `g_pit_ticks`. I had modelled the PIT clock as a 32 kHz crystal
÷ 4; the ROM actually programs SCCR = 0x03800000 (PIT clock = EXTCLK ÷ 512)
and PLPRCR[MF] = 0 (EXTCLK = SYSCLK = 48 MHz): 93.75 kHz, one PIT event per
4.03 ms. The PIT clock is now derived from SCCR like the hardware does.

**Fast-forwarding busy-waits.** Writing flash took ~70 s: half of it in
`delay_us`/`delay_ms`, which spin on `mftb`. `fastforward.rs` recognises the
three wait loops (after checking their exact instruction words), computes the
remaining ticks from the loop's registers, and advances the virtual clock in
one step — without ever jumping past a decrementer interrupt (it stops there,
the ROM takes the interrupt, comes back, and the rest is skipped). Virtual
time is unchanged; wall time for the flash write dropped from ~70 s to ~4 s,
and the boot from 6.9 s to 5.2 s. `--no-fast-forward` turns it off.

*Correction (chapter 15.8):* "virtual time is unchanged" was a claim, not a
measurement. A test that boots with and without the fast-forward and
compares everything at the same instruction (`rom_accelerators_are_exact`)
found three precision bugs, now fixed:
* it skipped *onto* the decrementer's tick instead of stopping one before;
* it skipped while an interrupt was already pending, losing 10 of 116 timer
  interrupts during boot;
* a skip of the 3-instruction `delay_ms` loop wasn't a whole number of
  iterations.

The same file now also replaces the ROM's byte-by-byte memory clear with a
bulk fill: the ROMMON boot dropped from 8 s to 0.1 s (32 MB) and from 26 s
to 0.2 s (128 MB).

## 8.7 Try it

```bash
mkdir tftp && python3 tools/mkimage.py tftp/hello.bin "Hello from my own image!"
emu/target/release/c1700emu --confreg 0x2100 --tftp-root tftp
```

```text
rommon 1 > IP_ADDRESS=10.0.2.15
rommon 2 > IP_SUBNET_MASK=255.255.255.0
rommon 3 > DEFAULT_GATEWAY=10.0.2.1
rommon 4 > TFTP_SERVER=10.0.2.2
rommon 5 > TFTP_FILE=hello.bin
rommon 6 > tftpdnld -r                  # run from RAM
...
program load complete, entry point: 0x80008000, size: 0x94
Hello from my own image!
```

Run `tftpdnld` without `-r` (answer `y`) to write it to flash, quit, and
start again with `--confreg 0x2102`: it boots from the persisted flash.
The whole story is the integration test `c1700/tests/net.rs`.
