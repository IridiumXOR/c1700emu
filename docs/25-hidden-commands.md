# 25. Hidden commands and menus in ROMMON and IOS

Both ROMMON and IOS have more commands than `help` and `?` admit. This
chapter finds them twice: statically, in their command tables (IDA), and
by trying them on the emulator, which also makes a good test of the
emulated hardware. Two scripts came out of it:

* `tools/rommon_session.py`: types a list of commands at the ROMMON prompt
  (or at an IOS prompt restored from a snapshot) and records the output of
  each, answering questions (`Password:`, `[y/n]`, `[confirm]`) from the
  list;
* `tools/ios_hidden_commands.py SNAPSHOT OUTDIR`: lists `?` in nine IOS
  modes with and without `service internal` and reports the difference.

## 25.1 ROMMON: the command table and `priv`

The monitor's commands are one table at **0xFFF2EEC0**: 55 entries of
`{name, handler, help, flags}`, followed by the count (0x37). `help`
(0xFFF07AF0) prints an entry only if `flags & 2 == 0` and, when
`flags & 1`, only once bit 3 of the monitor state byte (0x800064DC) is
set. The dispatcher refuses the same commands with
`monitor: command "dump" not found`. 28 commands are public, 26 are
**privileged**, and one, `priv`, is never listed.

`priv` (0xFFF07BEC) asks for a password: four hex digits, compared with
the **sum of the cookie's first five 16-bit words**. It is different on
every box (it depends on the MAC address); a wrong one is silently
ignored. With the emulator's default cookie (`01 01 02 17 00 00 00 01 33
00 ...`) it is 0x0101 + 0x0217 + 0x0000 + 0x0001 + 0x3300 = **3619**:

```
rommon 2 > priv
Password: 3619
You now have access to the full set of monitor commands.
Warning: some commands will allow you to destroy your
configuration and/or system images and could render
the machine unbootable.
```

(`cookie` prints the cookie; with `--mac` the password changes.)

| privileged command | what it does (tried on the emulator) |
|---|---|
| `addrloop`, `memloop`, `memdebug`, `tscope` | scope loops for a hardware engineer's oscilloscope (address walk, read/write loop, write/read/verify, timer pulse: `scope triggers every 40000 and 40 usecs`); they ask for their parameters and loop forever (`tscope` did not stop on BREAK) |
| `alter`, `dump [-dlbw] addr len`, `fill`, `ifill`, `move`, `compare`, `checksum` | memory editor: `dump 0xfff00100 0x40`, `checksum 0xfff00000 0x80000` → `Checksum is 0x719f` |
| `berrscan start size` | probes addresses for bus errors: `no bus error from location 0x68030000 to 0x6803ffff` (an empty WIC slot floats, chapter 19) |
| `call addr [args]`, `jump addr [args]`, `launch` | run code at an address / a downloaded image |
| `cat`, `fdump` | print or hex-dump a flash file |
| `cpu` | `PPC860 PowerQUICC, partnum 0x0005, version X02(0x0002)` |
| `echo` | echo, with `$VARIABLE` expansion |
| `errlog`, `clrerr` | the monitor's error log |
| `flash [-bd] info\|erase\|prog` | flash services: `flash base: 0x60000000  flash width: 2 ... device: INTEL 28F128J3A` |
| `initfs` | re-read the flash file system |
| `memtest [-qmp:r:] start len` | memory test until BREAK |
| `menu` | the diagnostic menu (25.2) |
| `sleep ms`, `speed`, `tcal value` | delay, a timed performance loop, timer calibration |
| `watchdog` | "test watchdog rebooting of the box" (not tried: the software watchdog is not modelled) |

Public commands have hidden options too, which `-?` reveals:

```
usage: boot [-nraxv] [filename] [ipaddr] [args]
-x  load but do not execute
-v  verbose
-r  debugger break in boot helper image
-n  debugger break in system image
-a  debugger break in both images
usage: xmodem [-cyrx] <destination filename>       (-c CRC-16, -y ymodem-batch, -r to DRAM, -x don't launch)
usage: dnld [-axv:up:] [args for downloaded pgm]   (-a via the AUX port, -u upload ...)
```

## 25.2 The diagnostic menu

`menu` opens a factory test menu, and every test runs on the emulator:

```
      Main Diagnostic Menu
a: alter diag flags                  j: size main memory
b: basic utilities                   k: main memory test
c: do all diags in this menu         l: main memory refresh test
d: do group of diags in this menu    m: flash memory test
e: bus error test                    n: aux loopback test
f: monitor image checksum test       o: aux port interrupt test
g: internal interrupt test           p: data cache test
h: ip state test                     q: mpc860 test
i: timer interrupt test              r: nvram test
```

| test | result on the emulator |
|---|---|
| e, f, g, h, i, l, p, q | `... completed, ... 0 errors` |
| j size main memory | `0 errors, found 32 mb` |
| m flash memory test | erases and rewrites every 128 KB sector: `0 errors` (it destroys the flash contents) |
| r nvram test | `0 errors, tested 32 KB` |
| k main memory test | phase after phase without errors (stopped after 46 phases; slow: use `--cpu jit`) |
| n, o AUX loopback tests | `Timeout on receive of first char - make sure loopback adaptor is installed`, as on a real router without the plug (not modelled) |

`a` toggles the flags (continuous, stop/loop on error, quiet). `b` is a
**Diagnostic Utilities Menu**: memory editor, bus error scan, loops,
console BREAK test, AUX echo test, an **MPC860 register dump** (`o`: SIU,
memory controller, timers, clocks, CPM; the values our model holds:
`memc_br6 : 0x60000901`, `memc_or6 : 0xff000190`...) and a **serial
cookie utility** (`n`), which edits the cookie field by field and so
documents its layout:

| bytes | field |
|---|---|
| 0x00 | version (01) |
| 0x01 | vendor (01) |
| 0x02-0x07 | Ethernet hardware address |
| 0x08 | processor (0x33 = MPC860) |
| 0x09 | NVRAM size (00 = 32 KB, 01 = 64 KB) |
| 0x0A | CPU speed (01 = 50 MHz) |
| 0x0C-0x0D | on-board PM ID (00 b2 = 1720) |
| 0x0E-0x0F | MAC addresses allocated |
| 0x18-0x22 | serial number |
| 0x23-0x24 | deviation |
| 0x2D | board config (04) |

## 25.3 ROMMON variables, aliases and the startup script

ROMMON keeps variables, aliases and the configuration in NVRAM (`sync`
writes them):

* **`PS1`** is the prompt; `!` becomes the command number. Default
  `PS1=rommon ! > `. A value with spaces needs quotes:
  `PS1="c1700-emu ! > "`.
* **`MONRC`** is a startup script: at power-on `monitor_main`
  (0xFFF070E0) runs it as a command line before the first prompt.
  Verified: `MONRC="meminfo"`, `sync`, `reset`, and the memory summary
  appears by itself after the banner.
  The emulator can set it from outside: `--rommon-var MONRC=meminfo`
  (chapter 7) gives the same at the very first power-on.
* **Aliases**: `r=repeat h=history ?=help b=boot ls=dir i=reset k=stack`
  are built in; `alias mi=meminfo` adds one (kept across resets).
* The `tftpdnld` variables (`IP_ADDRESS`, `IP_SUBNET_MASK`,
  `DEFAULT_GATEWAY`, `TFTP_SERVER`, `TFTP_FILE`, plus `TFTP_VERBOSE`,
  `TFTP_RETRY_COUNT`, `TFTP_TIMEOUT`, `TFTP_CHECKSUM`) are listed by
  `tftpdnld -?`.

**The configuration register utility** (`confreg` without an argument)
asks about every bit: diagnostic mode (0x8000), use net in IP broadcast
address (0x4000), load ROM after netboot fails (0x2000), use all-zero
broadcast (0x0400), BREAK/abort has effect (0x0100, inverted), ignore
system config info (0x0040), console baud rate, boot characteristics
(0x000F). Diagnostic mode is chapter 7.5b.

**BREAK during the boot** stops it even when the configuration register
disables BREAK (0x2102): sent while IOS decompresses itself, it gives
`monitor: command "boot" aborted due to user interrupt` and the ROMMON
prompt. That is the password-recovery path.

## 25.4 IOS: what `service internal` reveals

`tools/ios_hidden_commands.py` restores a snapshot at `Router#`, types `?`
in nine modes, enables `service internal`, types them again:

| mode | visible | more with `service internal` |
|---|---|---|
| user exec | 40 | 0 |
| privileged exec | 84 | 6: `analyze` (IOS resources, memory-leak analysis), `iterate-ip-addrs`, `ldap`, `loop-counter` (FOR_ALL_XXX IDB loop debugging), `processes` (`processes cpu pid`), `socktest` |
| `show` | 196 | 1: `coverage` (`% Coverage history is not enabled`) |
| `debug` | 177 | 2: `analyze`, `parser` (alias, ambiguity, cache, dump, help, http, interface-range, mode, privilege, rc, view) |
| `test` | 31 | 12: `cops`, `http`, `iec`, `ip`, `ipv6`, `javascript`, `memory`, `mrcp`, `parser` (`lock`/`unlock` config mode), `policy-manager`, `tcp`, `vxml` |
| `clear` | 65 | 1: `configuration` |
| global config | 176 | 1: `coverage` |
| interface, line config | 70, 70 | 0 |

Two of them deserve a warning. `test memory` with no argument asks
`Testing NVRAM card will remove all files! Continue [y/n] ?`, and the
answer typed by mistake decides. `loop-counter types` shows what the
internal ones are for:

```
  hw                 ON (0x00000001) FOR_ALL_HWIDBS
  sw                 ON (0x00000002) FOR_ALL_SWIDBS
  sw-with-addr       ON (0x00000010) FOR_SWIDBS_WITH_IPADDR
  ...
```

## 25.5 IOS: commands no help shows

How IOS decides is in its parser tables. Every keyword has a record in
`.rodata`: `{char *keyword, char *help, u32 *priv}` (the record of `show
coverage` is named `kw_show_coverage` in IDA), and the priv word in
`.data` encodes the visibility. Of 26 346 records in the image:

| priv word | count | meaning (checked against the listings above) |
|---|---|---|
| `0xFF000000` | 17 990 | ordinary keyword, the mode's privilege level |
| byte 1 bit **0x01**, e.g. `0xFF010000` | 184 | shown only with `service internal` (`analyze`, `coverage`, `loop-counter`...) |
| byte 1 bit **0x02**, e.g. `0xFF020000` | 52 | **never** shown by `?`, even with `service internal`, but accepted when typed |

Byte 0 is the privilege level (0xFF: the mode's own); other byte-1 bits
(0x04, 0x08, 0x10, 0x40) are common and don't hide anything.

The always-hidden ones include `test scheduler` (with `new`, `old`,
`event-mask`, `watched-message`), `test x25 dbit`, `test dsl`, the HTTP
and VXML statistics, and **`test voip`**, which opens a separate shell
(`voip_aaa_test_menu`, 0x81C82840):

```
Router#test scheduler ?
% Unrecognized command                       <- no help: the keyword is hidden
Router#test scheduler
% Incomplete command.                        <- but it exists
Router#test voip
voip-aaa 1> help
help                print help message
notify              notify status
exit                exit command session
voip-aaa 2> quit
Router#
```

`reload debug` (an internal keyword, "Break into debugger") reloads with
`Reload Reason: debug`; the next boot prints `Smart Init is disabled.
IOMEM set to: 25`, as in diagnostic mode, and no debugger break appears
on the console. The well-known hidden commands of older IOS releases,
`gdb` and `if-con`, are not in this image at all (no such strings).
`show region` and `show stacks` are ordinary, visible commands here.

## 25.6 The emulator under these tests

Everything the diagnostic menu and the hidden commands touched behaved
like the hardware: memory, flash (erase/program of every sector), NVRAM,
timers and interrupts, the bus monitor on unmapped addresses, the register
dump. Two things are not modelled and show up as such:

* the **AUX loopback adaptor**: the AUX tests fail exactly as on a real
  router without the plug;
* the **software watchdog**: `watchdog` was not run, because nothing
  would reset the box.

The names are in `tools/ida/c1700_names.py` (`monitor_command_table`,
every handler as `cmd_<name>`, `cmd_priv`, `cmd_menu`, `monitor_main`,
`g_monitor_flags`) and `tools/ida/ios_124_8_names.py`
(`voip_aaa_test_menu`, `voip_aaa_parser_init`, `kw_show_coverage`).
