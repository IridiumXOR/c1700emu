# 10. Towards full IOS emulation — what is missing

Dynamips (`ref/dynamips`, GNS3's C1700 emulator) is used as a map. Every
claim below is checked against what IOS actually did in our emulator, and
flagged when the two disagree. Two runs of IOS 12.4(8) adventerprisek9
are the evidence, both with `--stats`:

* **Run A** (first attempt): model 1720, `tftpdnld -r`, `--ram 128`. It
  reached the setup dialog after 637 s, printed `%C1700-3-BADIMAGE`, then the
  console stalled and there was no `Router>`.
* **Run B** (after the fixes in §10.2): model 1760, booted from the internal
  flash (`--flash-file`, chapter 11). **`Router>` after 287 s**, and `show
  version`, `show interfaces`, `show diag` and `show flash:` all answer.

## 10.1 What works under IOS (run B)

```
Cisco 1760 (MPC860T) processor (revision 0x00) with 114688K/16384K bytes of memory.
MPC860T processor: part number 5, mask 2
1 FastEthernet interface
32K bytes of NVRAM.
32768K bytes of processor board System flash (Read/Write)
System image file is "flash:c1700-adventerprisek9-mz.124-8.bin"
FastEthernet0/0 ... Hardware is PQUICC_FEC, address is 0217.0000.0001
```

* CPU: billions of instructions, **0 Software-Emulation, 0 Program
  exceptions**: no unimplemented instruction anywhere.
* Timers and interrupts: decrementer, PIT, SIU and CPM interrupt controller
  (16 964 external interrupts in run B).
* ROMMON services: 544 `sc` (EMT) calls answered by the *real* ROM
  (Dynamips fakes these).
* Memory controller, NVRAM, cookie: model, MAC and the `show diag`
  mainboard EEPROM are all built by IOS from our cookie.
* Flash: Intel 28F128J3 x16 × 2; IOS lists our flash file system.
* FastEthernet0/0: detected, link up, 15 frames transmitted while it was up.
* MMU: emulated (chapter 12). IOS pins 44 TLB entries and runs with
  translation on: 0 TLB misses per boot, read-only text enforced.

## 10.2 What blocked the prompt, and how each was fixed

| # | Piece | Evidence before → after | Dynamips |
|---|-------|-------------------------|----------|
| 1 | **DUART channel B (AUX)** at 0x68050008 | run A: our decoder (`offset & 7`) aliased AUX onto the console; IOS programming AUX reprogrammed the console, so output stopped. Run B: channel B r=421 w=51, console fine | channel B at +8 ✅ |
| 2 | **Platform identity** (cookie PM ID, `--model`) | run A: `BADIMAGE: ... platform ... is not voice capable` (PM ID 0x00B2 = 1720). Run B with 0x0316: gone | same PM ID table ✅ |
| 3 | **FPGA configuration port**, IO FPGA 0x4C (1760) | the 1760 hung right after decompression: loop waiting for INIT_B; then IOS bit-bangs a 166 980-byte Xilinx XC2S200 bitstream. Now configured, DONE raised (chapter 11.6) | returns 0xFF, "probably used to update the fpga" ✅ |
| 4 | **CPM SPI engine** + WIC/slot ID EEPROMs | see below: two bugs | ✅ EEPROMs read over SPI |
| 5 | **FEC**: graceful stop (X_CNTRL.GTS → I_EVENT.GRA), CPM interrupt controller | run A: ECNTRL written 10 071×, MII 273 000× (IOS re-initialising the interface endlessly). Run B: 151 and 360; FastEthernet0/0 detected | FEC ✅ |
| 6 | **PHY status** (LXT971 register 17) | `show interfaces` said "Half-duplex": our value 0x4C00 had the collision bit (0x0800) instead of full duplex (0x0200). Now 0x4780: verified, IOS reports "Full-duplex, 100Mb/s" | models an LXT970A (ID 0x7810) instead; the ROM checks for the LXT971's ID1 0x0013 |

**The SPI story, a lesson in checking your own explanations.** In run A,
IOS started 5 416 SPI transfers and read the DPRAM 88.7 million times. I
concluded "IOS polls a descriptor our missing SPI engine never completes"
and implemented the engine (`board/cpm_spi.rs`). The next run still showed
**90.8 million** DPRAM reads, and the SPI debug log explained why: 5 544
starts, **0 transfers**. IOS writes RBASE/TBASE = `0x2420/0x2428`. These are
offsets from IMMR, not from the dual-port RAM. The DPRAM is 8 KB (manual
§18.6), so the CPM uses only the low 13 bits and `0x2428` means DPRAM
offset 0x428. Our engine looked at DPRAM+0x2428, found no ready descriptor
and did nothing. With 13-bit decoding:

| | offset bug | 13-bit decode |
|---|---|---|
| SPI starts / transfers | 5 544 / 0 | 268 / 268 |
| DPRAM reads | 90 850 320 | 8 188 |
| "Unknown Card in slot 0…3" | printed | gone |
| setup dialog at | 297 s | 269 s |

So the first explanation was right, and the first fix was incomplete. Only
the per-transfer log showed it. The "Unknown Card" message disappearing is
an independent check: IOS's IOMEM sizing loop (0x80141340) prints it when
a slot's card type is not 0xFF (no card) and not a known type. The card
type comes from the EEPROM, so before the fix it came from never-written
receive buffers.

What the transfers look like: `03 00 <addr> 00`. Command 0x03 is READ on
SPI serial EEPROMs, then an address. The empty slot answers 0xFF.
Selects (IO FPGA reg 0x18) seen: 0x58, 0x60 (bits 0x08 and 0x20 = WIC 1
and 0 per Dynamips), and also 0x70, 0xD0, 0xD4, 0xD8, 0xF0, which
Dynamips doesn't document. They are presumably the 1760's slots 2/3 and
other devices; still to decode.

## 10.3 Known inaccuracies left (visible in run B)

| What | Evidence | Probable cause / next step |
|------|----------|----------------------------|
| ~~`Processor board ID 0000 (1314672220)`~~ | `show version` | fixed: the serial lives at cookie 0x18 in Cisco format (chapter 4); now `EMU1234C170` |
| ~~5 machine checks~~ | `--stats` exceptions | explained (chapter 19.3): deliberate probes past the end of RAM and flash, by ROMMON and by IOS's `probe_address`; faithful |
| ~~slot registers at 0x68030000, 0x68040000, 0x68080000, 0x680B0000~~ | each written twice with 0x05 and read twice | explained (chapter 19.1): the presence test for voice cards in the four slot windows; modelled as empty slots |
| ~~IO FPGA registers 0x0A, 0x0C, 0x1C, 0x5C, 0x6C~~ | first touched on the 1760 | decoded (chapter 19.2): slot 3 control, slot resets, voice TDM routing; latches are right without cards |
| IO FPGA 0x04 read 71 000×, 0x14 read/written 19 000×; WIC select 0x18 down from 10 974 to 414 accesses with SPI working | `--stats` CPLD table | 0x04 = board/CPU type (polled); 0x14 bits partly known (NVRAM write protect) |

## 10.4 For a complete router (after the prompt)

| Area | What | Why |
|------|------|-----|
| Networking | ✅ receive path (IDMA, chapter 13): IOS pings the virtual host. ✅ TAP devices and UDP tunnels (chapter 18) | telnet into the router from the PC, TFTP from IOS to a real server |
| WAN cards | CPM **SCC** in HDLC mode + WIC-1T/2T: EEPROM contents over SPI (now working), slot registers | serial interfaces |
| AUX port | channel B exists; give it its own TCP port | modem/aux line |
| Watchdog | software watchdog (IOS services SWSR ~42 000× per boot) | fidelity (not needed to run) |
| Speed | faster interpreter, or **snapshots** (save the machine once IOS is up, restore in seconds) | ~4.8 minutes to `Router>` today |

## 10.5 Not needed for a data router

Voice DSPs (`%IPM_C54X-3-NODSP: Can't get dsp resources`; PVDM slots "Not
populated" in `show diag`), the internal VPN/encryption module, Smart Init:
IOS reports them as absent and carries on.
