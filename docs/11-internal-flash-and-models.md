# 11. Booting from internal flash, and choosing the router model

So far every IOS run went through `tftpdnld -r`: 35 s of download over the
emulated Ethernet, every time. A real C1700 boots from its own flash. This
chapter covers which boot devices the platform actually has, how the flash
is built, how ROMMON finds an image in it, and how the emulator now starts
with an image already in place (`--flash-file`). The second half covers the
`--model` option. IOS decides which router it runs on from one field of the
NVRAM cookie, and some images refuse the wrong one.

## 11.1 Which boot devices does a C1700 have?

The question was: ROM, PCMCIA, SD/CompactFlash? The evidence:

* **The datasheet** (`cisco-1750-modular-access-router.pdf`, p. 6, "Flash
  memory"): *"Type: onboard (socketed) miniflash card; default: 4 MB;
  available sizes: 4, 8, 16 MB; maximum flash: 16 MB; support dual flash
  bank"*. It lists no PCMCIA slot and no CompactFlash.
* **Cisco's 1700 reference page**: 1750: *"One internal mini-flash card
  slot (max 16MB flash)"*; 1720: 8 MB/16 MB; 1721: 16 MB (non-upgradable);
  the voice bundles 1751-V/1760-V ship with **32 MB**. No other storage is
  mentioned for any model.
* **The ROM**, asked directly in the emulator:

  ```
  rommon 1 > dev
  Devices in device table:
          id  name
      flash:  flash
  rommon 3 > dir bootflash:
  bad device name
  rommon 4 > dir slot0:
  bad device name
  ```

  The ROM *contains* the strings `bootflash:`, `slot0:` and `PCMCIA`, but
  they belong to code shared by all ROMMONs (the generic loader and the
  register-dump labels `pcmcia_pbr0`…). This board's device table has
  one entry. The `--stats` IMMR report agrees: the ROM reads the PCMCIA
  pin register PIPR 3 times and writes PER once, but never programs a PCMCIA
  window (PBRx/PORx, PGCRx). Nothing could be mapped there.
* **IOS** reports only *"processor board System flash"* (12.3(11)T2:
  *"4096K bytes of processor board System flash (Read/Write)"*). Its
  `disk0:`/PCMCIA strings are, as in the ROM, shared code for other platforms.

So the **only** boot device besides the network is the internal
miniflash, a small socketed card with flash chips on it. "Boot from internal
flash" is all the platform needs.

## 11.2 What the flash chips are (and a decompiler lesson)

Chapter 4 first modelled the flash as two byte-wide AMD Am29F016 chips (2 × 2
MB). That is what ROMMON accepted first, but real C1700s don't use it:

* **Dynamips** models the C1700 flash as *"1x28F320 in word mode"*: one
  Intel StrataFlash chip, 16 bits wide.
* **The ROM agrees.** Its board-specific flash hook reports *bus width 2*
  and rotates every 32-bit ID read by 16 bits. An x16 Intel chip answers the
  "read identifier" command (0x90) with manufacturer `0x0089` at word 0 and
  the device code at word 1. One 32-bit read sees `0x0089_00DD`; after the
  rotate it is `0x00DD_0089`; ROMMON then forms *mandev* = `(byte 3 << 8) |
  byte 1` = `0x89DD`. Its chip table contains `0x8916/0x8917/0x8918`, which
  are Intel 28F320J3/28F640J3/28F128J3.
* **IOS 12.4(8)** has the same parts in its flash table: "INTEL 28F320J3",
  "INTEL 28F640J3", "INTEL 28F128J3" (16 MB, the largest J3).

**How I first got this wrong.** Reading Hex-Rays' output for the ID code, I
took `BYTE1(v)` to mean bits 8–15 and concluded the Intel path could never
match. The Hex-Rays macros index **memory bytes**: `BYTEn(x)` is "the byte at
offset n of x in memory". On a big-endian CPU byte 1 is bits **16–23**. With
the right reading the arithmetic gives `0x89DD`, and an end-to-end test
confirms it: ROMMON's `tftpdnld` writes to an Intel x16 model and boots the
result (`tests/net.rs`). The lesson: check what a decompiler macro means on
your target's endianness before you build a conclusion on it, or read the
raw `rlwinm` instructions.

The emulator's flash (`emu/c1700/src/board/flash/`) is now a choice of two
models:

| `--flash-size` | chips | model | ROMMON/IOS see |
|----------------|-------|-------|----------------|
| 4  | 2 × Am29F016 (x8 pair) | `amd.rs` | the original model, kept for comparison |
| 8  | 1 × 28F640J3 x16 | `intel.rs` | "INTEL 28F640J3" |
| 16 | 1 × 28F128J3 x16 | `intel.rs` | default for 1720/1721/1750 |
| 32 | 2 × 28F128J3 x16 (two banks) | `intel.rs` | default for 1751/1760 (as the -V bundles ship) |

`intel.rs` implements the Intel command set as a small state machine per
chip (bank). The commands are read array, read ID, CFI query, read/clear
status, word program, write-to-buffer, block erase (128 KB blocks) and the
lock bits, which are accepted and ignored. Program and erase finish
instantly, leaving the chip in read-status mode with status `0x80` (ready),
which the drivers poll for. Programming only clears bits (`old & new`), as
on real flash. A driver that "programs" without erasing gets the same wrong
result as on hardware.

The flash window only decodes as flash for `offset < size`. Above that the
bus reads like an empty bank (see chapter 4: a mirrored window made ROMMON
"find" eight banks and overflow its stack).

## 11.3 How ROMMON finds the image: the flash file system

Chapter 8 showed the file header ROMMON writes after `tftpdnld`. To put an
image in flash *without* ROMMON, the emulator must write the same format.
From ROMMON's writer (0xFFF1DDB0) and reader (`find_partition`):

```text
+0x00 u32  0xBAD00B1E      magic
+0x04 u32  length          of the data
+0x08 u16  ~csum16(data)   one's complement of the one's-complement 16-bit sum
                           (the Internet checksum)
+0x0A u16  0xFFFF          flags
+0x0C u32  ????            ROMMON never writes it: whatever was on its stack
+0x10 char name[48]
+0x40 data …, next header at 0x40 + round_up(length, 4)
```

An erased word (`0xFFFFFFFF`) where the next magic should be ends the list.

`board/flashfs.rs` has `build(files, size)` (lays files out exactly like
that) and `list(image)` (walks them and verifies each checksum). Its golden
test compares against the header ROMMON itself wrote for a 312-byte test
file (checksum `0x8f5d`), so the two implementations can't drift apart. The
only field we can't match is the one ROMMON leaves as stack garbage.

**Which file does ROMMON boot?** With the configuration register's boot
field = 2..15 (e.g. `0x2102`) and no `BOOT` variable, ROMMON loads the
**first file** in flash, checks the Cisco image rules from chapter 8 (ELF
`e_machine` 0x33, FEEDFACE trailer, checksums), copies the sections to RAM
and jumps to the entry point. A `-mz` image then decompresses itself.

## 11.4 Using it

```bash
emu/target/release/c1700emu --model 1760 --ram 128 --confreg 0x2102 \
    --flash fl1760.bin --flash-file roms/c1700-adventerprisek9-mz.124-8.bin
```

* `--flash-file FILE` (repeatable) **replaces** the flash content with a
  fresh file system holding those files, in order, and saves it to `--flash`.
  The next run can omit `--flash-file` and boots the same flash file.
* The log line at start shows what is in flash, with checksums verified:
  `model 1760 (PM ID 0x0316), flash 32 MB: 2 x Intel 28F128J3 (x16): c1700-adventerprisek9-mz.124-8.bin (21299736 bytes)`.
* `--flash-size` overrides the model's default (above).

What you see: no download, just
`program load complete, entry point: 0x80008000, size: 0x14500a0`, then
`Self decompressing the image : ####…`.

Verified by `tests/boot.rs::boots_image_preloaded_in_internal_flash`, which
builds a flash with `flashfs::build`, powers on with confreg `0x2102` and
waits for the test image's message.

## 11.5 `--model`: the mainboard PM ID

The first IOS 12.4(8) run printed

```
Cisco 1720 (MPC860T) processor ...
%C1700-3-BADIMAGE: The IOS image loaded supports voice.  The platform you are running is not voice capable.
```

Where does IOS get "1720"? Following the string "C1720 1FE Mainboard" back
in the decompressed IOS led to a `switch` (at 0x8013F598) on a 16-bit value
read from the **NVRAM cookie, bytes 0x0C–0x0D: the on-board "PM ID"**. The
ROMMON cookie editor even prints the recommended value, *"00 B2"*, which is
what our default cookie had. The cases of that switch:

| model | PM ID | voice |
|-------|-------|-------|
| 1720 | `0x00B2` | no |
| 1721 | `0x035A` | no |
| 1750 | `0x00C9` | yes |
| 1751 | `0x024D` | yes |
| 1760 | `0x0316` | yes |

Dynamips' `c1700_mainboard_id` table has exactly the same numbers, so the
table is confirmed twice from independent sources. *Trust Dynamips, but
verify.*

`--model` (default 1720) writes the PM ID into **both** cookie copies (primary
at NVRAM+0 and backup at +0x65A) and recomputes each copy's checksum, since the ROM validates the cookie with
that checksum (`cookie_is_valid` 0xFFF0B1D8). It also picks the default
flash size. `adventerprisek9` is a voice image, so it
needs 1750/1751/1760.

## 11.6 The 1760 loads an FPGA at boot (IO FPGA register 0x4C)

With `--model 1760` the image decompressed and then IOS went silent. The
1720 run had printed "Smart Init is disabled…" soon after decompression.
Two pieces of evidence located the hang without guessing:

1. **The log**: the 1760 run reached IOS's first MMU setup (`mtspr 796`
   at 0x80130d18, same as the 1720 run) but never the WIC-slot probe that
   followed on the 1720. The hang was somewhere in between.
2. **A GDB sample**: start the emulator with `--gdb`, `continue`, interrupt
   a few minutes after decompression, dump the registers and the code around
   the PC (`tools/gdb_sample.py` automates this):

   ```
   r28 = 0x68020000 (IO FPGA)     r27 = r30 = 0x316 (the 1760's PM ID)
   80517d10: lbz   r0,76(r28)     ; IO FPGA register 0x4C
   80517d14: andi. r9,r0,8        ; bit 3
   80517d18: beq   80517d10       ; until it is 1, no timeout
   ```

Disassembling the routine (`0x80517c68`, plus the helper `0x80517c38`
called once per bit) shows a textbook **Xilinx "slave serial"
configuration**: the CPU bit-bangs an FPGA bitstream through one CPLD register.

| bit | dir | Xilinx pin | what IOS does |
|-----|-----|------------|---------------|
| 0x40 | out | PROG_B | pulse low, delay, high: clear the FPGA |
| 0x08 | in | INIT_B | wait for 1 (the hang: we returned 0) |
| 0x20 | out | CCLK | per bit: low, put DIN, high |
| 0x10 | out | DIN | data, MSB of each byte first |
| 0x02 | in | DONE | after the stream (+1 clock): 1 = configured, else print an error and retry, max 4 |

The buffer IOS passes (0x83426FD0, 166 980 bytes) starts with `FFFFFFFF
AA995566`, the Xilinx dummy and sync words. Parsing its packets
(`tools/xbit.py`; `tools/iosdis.py` disassembles IOS code) gives RCRC, FLR = 0x11, COR, FAR, a long FDRI write (the
frames), CRC, LFRM, **START**, CTL, CRC, filler. FLR = 0x11 means 18-word
(576-bit) frames. A Spartan-II frame is 18 × (rows + 2) + 32 bits rounded
up to 32, and for the XC2S200's 28 rows that is 572 → 576. The stream length
also matches the XC2S200's bitstream size, so the 1760 carries a **Spartan-II
XC2S200**. The image holds a second, smaller bitstream (FLR = 0x0D,
XC2S100-sized) for another board variant. Loading the FPGA is
model-specific: in the 1720 run IOS never touched register 0x4C (the
`--stats` CPLD table has no entry for it). Inside the routine, PM IDs 0x24D
(1751) and 0x316 (1760) get one extra clock at the end.

**Model** (`board/fpga_config.rs`): INIT_B follows PROG_B; CCLK rising edges
shift DIN into a 32-bit register until the sync word appears, then the
packets are walked (type 1: register + count; type 2: long count). DONE rises
when the CMD register receives START. On a real part, START begins the
startup sequence that releases DONE a few clocks later, during the filler
words. Walking the packets matters: a START-like word inside frame
data must not count, and a unit test checks exactly that. The log shows:

```
FPGA configured over IO FPGA reg 0x4C: 166980 bytes clocked in, frame length 18 words, ... START -> DONE
```

**Dynamips** returns 0xFF for this register ("Unknown, read on 1760.
Considering the activity pattern, it's probably used to update the fpga").
That works because INIT_B and DONE read as 1, and its guess was right.
The disassembly now shows what each bit means.

## 11.7 Where the code is

| file | what |
|------|------|
| `board/flash/mod.rs` | `Flash` enum (AMD/Intel), backing file + persistence, `for_size_mb` |
| `board/flash/intel.rs` | Intel StrataFlash x16 command set, CFI, tests (incl. the ROM-style ID read) |
| `board/flash/amd.rs` | the earlier AMD Am29F016 pair |
| `board/flashfs.rs` | Cisco flash file system: `build`, `list`, `checksum`, golden test |
| `board/cookie.rs` | `MODELS`, `set_pm_id` |
| `board/fpga_config.rs` | IO FPGA reg 0x4C: Xilinx slave-serial configuration port |
| `main.rs` | `--model`, `--flash-size`, `--flash-file` |
