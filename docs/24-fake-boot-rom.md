# 24. A boot ROM of our own: `--rom builtin`

Every chapter so far boots IOS the way a real 1700 does: Cisco's boot ROM
(ROMMON 12.2(1r)XE1) runs first, sets the machine up, finds the image in
flash, loads it and jumps to it, and IOS keeps calling back into the ROM
while it runs. Without the 512 KB `CISCO_bootrom_AT27C040.bin` nothing
boots. Dynamips (chapter 10) never needed one, because it brings its own
stand-in. This chapter does the same: it finds out what IOS really needs
from the ROM and builds a minimal ROM inside the emulator that provides
just that:

```
$ c1700emu --rom builtin --model 1760 --ram 128 --flash-file roms/c1700-adventerprisek9-mz.124-8.bin

c1700emu built-in boot ROM, Version 1.0 (no ROM monitor)
C1700 platform with 131072 Kbytes of main memory
program load complete, entry point: 0x80008000, size: 0x14500a0
Self decompressing the image : ######################## [OK]
...
Router#show version
Cisco IOS Software, C1700 Software (C1700-ADVENTERPRISEK9-M), Version 12.4(8), RELEASE SOFTWARE (fc1)
...
ROM: c1700emu built-in boot ROM, Version 1.0 (no ROM monitor)

Router uptime is 1 minute
System returned to ROM by power-on
System image file is "flash:c1700-adventerprisek9-mz.124-8.bin"
```

The real ROM stays the default, and nothing changes when it is used. The
code is `emu/c1700/src/fakerom.rs`. The ROM functions named below are in
`tools/ida/c1700_names.py`, and the IOS ones in
`tools/ida/ios_124_8_names.py`.

## 24.1 What the ROM hands over

The first question is what the machine looks like when IOS's first
instruction runs. The GDB stub (chapter 7.3) answers it exactly: boot with
the real ROM, put a breakpoint on the image's entry point and dump
everything:

```
c1700emu --rom roms/CISCO_bootrom_AT27C040.bin --model 1760 --ram 128 \
    --flash-file roms/c1700-adventerprisek9-mz.124-8.bin --confreg 0x2102 --idle off --gdb 34567
gdb -ex "target remote :34567" -ex "break *0x80008000" -ex continue -ex "info registers"
```

| what | value | why |
|---|---|---|
| PC | 0x80008000 | ELF `e_entry`: IOS runs at the RAM alias 0x8000xxxx |
| r1 | 0x800063F4 | the ROM's own stack (the decompressor sets its own at once) |
| r3, r4 | 2, 0 | a "boot code" and no argument string |
| r5 | 0x800046BC | a parameter block in the ROM's data |
| r6 | 0x80008000 | the entry point again |
| LR | 0xFFF03D50 | inside `launch_image`: returning from the image is "back to ROM" |
| MSR | 0x9002 | EE, ME, RI. IP = 0, so exception vectors are **in RAM**; translation off |
| IMMR | 0xFF000000 | the internal registers |
| BR0/OR0 | 0xFFF00401 / 0xFFF005A6 | CS0: the boot ROM, 8 bits wide |
| BR1/OR1, BR2/OR2 | 0x00000081 / 0x7C000600, 0x040000C1 / 0x7C000600 | CS1-2: the two 64 MB SDRAM banks, through UPM A and UPM B |
| BR6/OR6 | 0x60000901 / 0xFE000190 | CS6: the flash, 16 bits wide |
| BR7/OR7 | 0x68000401 / 0xFFF001A8 | CS7: the I/O bus (NVRAM, CPLD, UART, slots) |
| SIUMCR, SYPCR, SIMASK | 0x00230440, 0xFFFFFF88, 0x20000000 | watchdog off; only IRQ1 (the console UART) unmasked |
| MAMR = MBMR, SCCR, PLPRCR, TBSCR | 0x0C804111, 0x03800000, 0x000050C0, 0x0001 | SDRAM refresh, clocks, timebase running |

One detail matters more than the others. The SDRAM banks' option
registers leave address bit 0 (0x80000000) out of the comparison, so
**RAM answers at 0 and at 0x80000000**. IOS's sections are linked at
0x8000xxxx and run there with translation off long before IOS turns its
MMU on (chapter 12). The memory controller does the aliasing, not the
MMU.

The rest is the image itself. The 12.4(8) file is an ELF file with
`e_machine` 0x33 (the ROM's `g_platform_code` for a C1700 and its check
that an image is meant for this platform), four allocated PROGBITS
sections (0x14500A0 bytes) and a NOTE section that is not loaded.
`loadprog` copies the sections to their addresses and `launch_image`
jumps to the entry. What runs first is IOS's own self-decompressor
("Self decompressing the image : ####"), which unpacks the real IOS and
starts it.

## 24.2 ROM monitor calls ("EMT" calls)

After the jump, IOS keeps using the ROM through **EMT calls** (the name
comes from the 68k Cisco boxes, where they were `emt` traps). On
PowerPC an EMT call is a `sc` instruction with the call number in r3
and arguments in r4-r6. With MSR[IP] = 0 the system call exception goes
to the *RAM* vector at 0xC00. The ROM put a stub there that jumps to
`exc_Syscall` (0xFFF0338C). That code saves the context, calls
`emt_dispatch(r3, r4, r5, r6)` (0xFFF161E8, a 73-case switch), puts the
result in r3 and returns with `rfi`. IOS's side is `emt_call`
(0x806154B4), with one tiny wrapper per call number (`emt_putc`,
`emt_memsize`, ... at 0x81BCDE8C-0x81BCEAA4).

To see which calls IOS uses and what the ROM answers, the CPU core now
has a `syscall` log target. Each `sc` and the matching `rfi` are logged
with the registers:

```
c1700emu ... --log emu.log --log-level warn,syscall=debug
[  4.733801 DEBUG syscall] sc at 0x806154b4: r3=0x2e r4=0x82711b48 r5=0x8000639c r6=0x14 lr=0x81bce610
[  4.733872 DEBUG syscall] sc returns r3=0xffffffff to 0x806154b8
```

A boot to the prompt plus a few `show` commands makes **544 calls of 22
kinds**. Each call below was read in `emt_dispatch`, and each real-ROM
answer is the one the trace shows:

| EMT | IOS wrapper | calls | meaning (ROM code) | real ROM answers | built-in ROM |
|---|---|---|---|---|---|
| 1 | `emt_putc` | 459 | console output (`emt_putc`: LF becomes CR LF) | 0 | the same, straight into the UART's output |
| 2 | `emt_rom_level` | 1 | ROM monitor "level" | 0xC00 | 0xC00 |
| 4 | `emt_memsize` | 8 | main memory size; the first call also clears RAM from 0x8000 | 0x08000000 | the banks' total |
| 6 | `emt_nvram_confreg_inv` | 1 | NVRAM confreg, complemented; -1 if the copy at 0x92 or the magic is bad | 0xDEFD | the same (`nvram_config::confreg`) |
| 7 | `emt_platform` | 13 | platform code, = ELF `e_machine` | 0x33 | 0x33 |
| 10 | `emt_rom_banner` | 3 | the banner: `show version` prints its first line after "ROM: " | "\r\nSystem Bootstrap, Version 12.2(1r)XE1..." | "\r\nc1700emu built-in boot ROM, Version 1.0..." |
| 13 | `emt_boot_data` | 1 | address of `boot_data[20]`: {19 (BOOT_COUNT), reason, PC, address} | 0x800065AC | 0x80004000 |
| 24 | `emt_nvram_size` | 3 | NVRAM size | 0x8000 | the same |
| 26 | `emt_boot_info` | 1 | {device type, "flash:NAME"} of the booted image | 0x80007260: {3, "flash:c1700-...bin"} | the same at 0x80004100 |
| 28 | `emt_boot_confreg` | 2 | the configuration register the image was booted with | 0x2102 | the same |
| 34 / 35 | `emt_image_string_get/set` | 1 / 1 | a 257-byte string IOS stores and reads back | 0x800072D4 / 1 | 0x80004400 / 1 |
| 36 / 37 | `emt_version_string_get/set` | 2 / 1 | the same for a version string, stored once per boot | 0x8000715C / 1 | 0x80004600 / 1 |
| 44 | `emt_cookie_copy` | 2 | copy the 128 cookie bytes (NVRAM + 2): MAC, serial, model | 0 | the same |
| 45 | `emt_monvar_set` | 2 | set ROMMON variable "NAME=VALUE" and save it in NVRAM | room left of 768 bytes (0x2EA after `BSI=0`) | the same, in NVRAM |
| 46 | `emt_monvar_get` | 23 | get a variable: its length, -1 if unset, -2 if too long | -1 (unset) | the same, from NVRAM |
| 47 | `emt_nvram_ios_offset` | 7 | where IOS's part of the NVRAM starts | 0x800 | 0x800 |
| 60 | `emt_iomem_get` | 3 | IOMEM percentage, NVRAM 0x9F ("IOMEM set to: 25") | 0x19 | the same; 25 if implausible |
| 62 | `emt_console_baud_get` | 1 | console speed from the NVRAM confreg | 9600 | the same |
| 71 | `emt_nvram_byte_a0_get` | 1 | NVRAM byte 0xA0 | 0 | the same |
| 132 | `emt_memsize` | 8 | a newer ROMs' memory size call; on -1 IOS (and its decompressor) asks EMT 4 | -1 (not supported) | -1 |

The variables IOS sets are `BSI=0` and `RET_2_RCALTS=` at every boot and
`RANDOM_NUM=...` (the crypto code's seed) later. The reload reasons in
`boot_data` are those of `boot_data_set` (ROM) and
`format_reload_reason` (IOS): 0 power-on, 1 abort, 2 breakpoint, 3 exit
trap, 4 trace trap, 5 emt, 6 bus error, 7 address error, 8 watchdog,
**9 reload**, 0xA/0xB parity errors, 0xC unexpected interrupt, 0xD write
bus error.

`emt_dispatch` has more cases that IOS doesn't call here. The built-in
ROM implements the cheap ones the same way, because IOS commands can
reach them: 8 (**reload**: reason 9 into `boot_data`, then the ROM's
reset), 25 (the string "C1700"), 59 and 72 (set NVRAM bytes 0x9F and 0xA0
and re-checksum the monitor area), and 61 (set the console speed bits of
the NVRAM confreg). The real ROM's "EMT police" cases (0, 5, 11, 12, 15,
16, 19, 29-31, 38, 42) print `ROM Monitor EMT police: EMT call "..."
requested but not supported` and return -1. Everything else returns -1
too.

## 24.3 Design: a ROM image that is mostly hooks

`--rom builtin` builds a 512 KB image (`fakerom::image()`) where the boot
ROM sits, 0xFFF00000. It is almost empty:

| address | content | serviced by |
|---|---|---|
| 0xFFF00100 (reset) | `b .` | `FakeRom::boot`: everything ROMMON does up to the image's first instruction |
| 0xFFF00C00 (system call) | `rfi` | `FakeRom::emt` answers in r3, then the `rfi` returns to the caller |
| 0xFFF00900 (decrementer) | `rfi` | nothing to do |
| the other vectors, 0x200-0x1F00 | `b .` | "unexpected exception at vector ..., SRR0=...": message, machine stopped |
| 0xFFF00080 (the image's LR) | `b .` | "the image returned": message, machine stopped |
| 0xFFF02000, 0xFFF02100 | the banner, "C1700" | EMT 10, EMT 25 |

These addresses are **hooks** of `Machine::before_step`, the mechanism the
ROM accelerators and the idle detector already use (chapters 15.5, 16.4,
16.7). The test `fakerom::is_hook` is a page comparison and `pc & 0xFF == 0`,
so it costs nothing on ordinary instructions. Translated JIT code ends
before a hook address, and the interpreter's batches stop there, so both
engines reach the Rust code with the CPU state exact.

**Boot** (`FakeRom::boot`) does in Rust the part of the real ROM's work
that IOS depends on:

1. Program the SIU, the clocks and the memory controller with the values
   of 24.1. CS1-CS4 get one SDRAM bank each (mask without bit 0), laid out
   from 0 as `dram_init_and_size` does it: the largest bank first, CS1 and
   CS4 on UPM A, CS2 and CS3 on UPM B. CS6 gets the flash and CS7 the I/O
   bus.
2. Print the banner and the memory size.
3. Take the configuration register from NVRAM (0x2102 if the NVRAM has
   none). Boot field 0 would mean "stay in ROMMON", which this ROM
   doesn't have, so it says so and stops.
4. Take the **first file in flash** (`flashfs::list`, chapter 8). The
   real ROM does the same when no BOOT variable is set.
5. Check the ELF file (32-bit big-endian, `e_machine` 0x33) and collect
   its allocated PROGBITS sections (`fakerom::elf_sections`).
6. Clear RAM from 0x8000 up (a reload leaves the old IOS there) and copy
   the sections in with `Board::load_dram`. Like `fill_dram`, it tells the
   JIT which pages changed (chapter 17.7).
7. Put the exception vectors in RAM. Each is `ba 0xFFF00v00`, an
   *absolute* branch, so it stays valid wherever IOS copies it.
8. Fill in `boot_data` (19, the reason: 0 at power-on, 9 after a
   reload), the boot info "flash:NAME" and the empty strings, then set
   r1, r3-r6, LR and MSR = ME | RI, and jump to the entry.

**EMT calls** (`FakeRom::emt`) are a `match` on r3 that follows the table
of 24.2. An unknown number answers -1, like the real ROM, with a warning
on the `syscall` log target the first time it is seen:

```
WARN  syscall] built-in boot ROM: EMT call 99 (r4=0x0) not implemented, returning -1
```

**State** lives where the real ROM keeps it, so the two ROMs can share
an NVRAM file. The monitor variables (45/46), the IOMEM percentage
(59/60), byte 0xA0 (71/72) and the console speed (61/62) are in the
NVRAM monitor area, through `board::nvram_config`, which also handles
`--rommon-var`, `--confreg` and `--nvram-info` (chapter 7.7). Like the
real `emt_monvar_set`, a variable is written to NVRAM at once, checksum
included (`monitor_area_sync`). The structures IOS reads through
pointers (`boot_data`, the boot info, the two strings) are in low RAM at
0x80004000, under the image's 0x80008000, which IOS leaves alone because
it belongs to the ROM. The real ROM keeps its own at 0x800065AC-0x800073D4. The
next reload reason, the boot confreg and the "version string already
set" flag are fields of `FakeRom`, and snapshots save them (chapter 20).

**Reload**: IOS's `reload` ends with EMT 8. The real ROM then records
reason 9 and resets the board through `cmd_reset` (a checkstop reset,
chapter 3.5). The built-in ROM returns `Action::Reload`, and the machine
does the same internal hard reset (`Machine::hard_reset(rsr::CSRS)`, RAM
kept). The CPU comes back at 0xFFF00100, so `boot` runs again, and IOS
then prints "System returned to ROM by reload".

## 24.4 Measurements

Same build, same machine, IOS 12.4(8) on a 1760 with 128 MB, fresh NVRAM
(so IOS shows the setup dialog). The times come from the session driver
(`tools/ios_session.py`): until the dialog, and until `Router>`. Each
pair ran with the same build:

| CPU engine, idle mode | real ROM: dialog, prompt | built-in ROM: dialog, prompt | saved |
|---|---|---|---|
| JIT, `--idle off` | 18.0 s, 19.7 s | 17.4 s, 18.9 s | 0.8 s |
| JIT, default (`--idle realtime`) | 144.5 s, 153.5 s | 119.9 s, 128.8 s | **24.7 s** |
| interpreter, `--idle off` | 184.5 s, 197.3 s | 162.9 s, 175.6 s | 21.7 s |

The default mode gains the most. In realtime mode the guest clock may
not run ahead of the wall clock (chapter 15). The real ROM's code uses
about 25 s of *guest* time (time the emulated CPU spends executing, like
on a real router), and that time is waited out later while IOS idles.
The built-in ROM uses no guest time.

The time goes to IOS, not to the ROM. The ROM's own part (from the
start of the process to the image's first EMT call) shrinks from
0.89 s to 0.22 s with the JIT, and from 16.6 s to about 0.2 s with the
interpreter. With the interpreter, what costs is the ROM's work on the
21 MB image: checksumming it and copying it out of the 16-bit flash.
The decompressor (3.8 s with the JIT) and IOS's initialisation take
the same time on either ROM. The EMT traffic is the same as well: same
calls, same counts, same answers apart from the addresses in 24.2.

## 24.5 Tests and limits

* `fakerom::tests`: each EMT handler against a board: memory size,
  console output with CR LF, confreg and console speed in NVRAM (EMT 61
  rewrites the speed bits and keeps the confreg valid), cookie copy,
  strings, monitor variables written to and read from the NVRAM monitor
  area, NVRAM bytes 0x9F/0xA0, unknown calls answering -1 and warning
  once, reload. Also ELF section selection and the image layout.
* `c1700/tests/fakerom.rs`: no Cisco file needed. A test image of 18
  hand-assembled instructions, put in flash as an ELF file, is booted by
  the built-in ROM. It prints "OK" through EMT 1, reads its boot reason
  through EMT 13, reloads through EMT 8, and after the second boot
  (reason 9) stores the memory size from EMT 4. The test checks the
  console (two "OK"s), one reset and the stored value. A second test
  checks the message for an empty flash.
* `tools/tests/fakerom_test.sh [options]`: IOS end to end. Fresh NVRAM,
  `show version` (ROM line), configure FastEthernet0/0, ping the virtual
  host 10.0.2.2, `hostname fakerom`, `write memory`, `reload`. After the
  reload the prompt must be `fakerom>` and the reason "reload". Then a
  **new emulator process** on the same NVRAM file must boot to
  `fakerom>` with reason "power-on". It skips when the IOS image is
  missing (`IOS_IMAGE`, `C1700EMU` override the paths). It passes in
  57 s with the JIT:

  ```
  boot1.log:[driver    17.8s] setup dialog: '[yes/no]:'
  boot1.log:[driver    19.2s] prompt: 'Router>'
  boot1.log:System returned to ROM by power-on
  boot1.log:Success rate is 80 percent (4/5), round-trip min/avg/max = 1/1/1 ms
  boot1.log:System returned to ROM by reload
  boot2.log:[driver    12.3s] setup dialog: 'Press RETURN'
  boot2.log:[driver    13.2s] prompt: 'fakerom>'
  boot2.log:System returned to ROM by power-on
  fakerom test: OK
  ```

  With the interpreter the same IOS session (prompt, `show version`,
  ping) works too, as do a snapshot taken at the prompt and restored in
  a new process (with `--rom builtin` again).

What the built-in ROM does **not** do:

* **No ROM monitor.** There is no `rommon 1 >`: no `boot` command, no
  `tftpdnld`, no break key, no `confreg` dialog. Boot field 0 is refused,
  and every other boot field boots the first file in flash. The BOOT
  variable and `boot system` are not consulted, and there is no
  network boot.
* No power-on self-test, memory test or image checksum (the real ROM
  checksums the flash file, `bootdev_checksum`, chapter 22), and no
  version-specific checks of the image beyond `e_machine`.
* The image is entered with MSR[EE] = 0, where the real ROM has EE = 1
  (for its own console interrupt). Nothing in the built-in ROM could
  answer an interrupt, and IOS enables them itself once its own
  vectors are in place.
* EMT calls IOS 12.4(8) doesn't make here are not implemented: 3/20
  (wait for a console character: it would have to block), 9, 21 (CPM
  reset), 33 (pointer to the ROM's cookie copy), 43, 48. They answer -1
  with the warning above. Another IOS version may need more of them,
  and the warning names the call to look up in `emt_dispatch`.
* A snapshot of a machine running the built-in ROM must be restored with
  `--rom builtin` (the ROM's three fields are in the stream only then),
  and the other way round.

Dynamips does this job with a "ROMMON emulation microcode": a small
PowerPC program shipped as a binary (`ref/dynamips/stable/ppc32_microcode`)
that answers the system calls as guest code ("unhandled syscall 0x%x at
pc=0x%x"), while the host loads the ELF file and turns translation on
with BAT registers (which an MPC860 doesn't have). Here the calls are
answered in Rust, and the machine starts in the state a real ROM leaves,
so the rest of the emulator (MMU, memory controller, NVRAM layout) runs
exactly as with Cisco's ROM.
