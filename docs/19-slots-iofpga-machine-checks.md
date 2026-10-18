# 19. Slots, the IO FPGA and the boot-time machine checks

Chapter 10 left three loose ends from IOS's first boots: four "unknown
devices" on the I/O bus, IO FPGA registers of unknown meaning, and five
machine checks during every boot. All three were cleared up in IDA on the
decompressed IOS 12.4(8) image (`ida-ios/`), and the names are in
`tools/ida/ios_124_8_names.py`. How it was found: a scan of the code for
`lis rX, 0x6802`/`0x6803`/`0x6804`/`0x6808`/`0x680B` (the high halves of
the addresses), then decompiling the functions that use them.

## 19.1 The WIC/VIC slots

The "unknown devices" at 0x68030000, 0x68040000, 0x68080000 and
0x680B0000 are the **slot windows** of the I/O bus (chip select 7), one
64 KB window per slot:

| slot | window | present on |
|---|---|---|
| 0 | 0x68030000 | all models |
| 1 | 0x68040000 | all models |
| 2 | 0x68080000 | 1750, 1751, 1760 (and PM ID 0x496) |
| 3 | 0x680B0000 | 1760 only |

`wic_slot_count` (0x80504F1C) gives the number per mainboard PM ID: 2 for
the 1720/1721, 3 for the 1750/1751, 4 for the 1760. (Dynamips puts slot 1
at 0x68031000; IOS uses 0x68040000, and IOS wins.)

**The presence test** (`slot_probe_voice_cards`, 0x80140A08) is what
produced the "written twice with 0x05, read twice" pattern:

```c
for (i = 0; i < wic_slot_count(); i++) {
    *(u8 *)base[i] = 0x05;            // write a pattern
    busy_delay(50);
    if (*(u8 *)base[i] == 0x0A)       // read back the low nibble inverted
        slot_voice_present |= 1 << i;
}
```

A card with its own logic (a voice interface card) answers the inverted
nibble; an empty slot drives nothing, so the read floats to 0xFF and the
test fails. The result matters for more than `show diag`: it selects the
length of the FPGA "firmware" image IOS registers (`fpga_firmware_register`,
0x80123944) and enables DSP resources. Data cards (WIC-1T, WIC-1ENET...)
are recognised by their **ID EEPROM** over SPI instead (chapter 10.2), and
a status register at offset 5 of the window reports their interface
state (`wic_slot_status_poll`, 0x80396348: bits 0x20/0x40 for card types
0x3FA-0x3FD, 0x473, 0x474).

**In the emulator** (`board/iobus.rs`) the four windows are now modelled
as what they are without cards: empty slots. Reads return 0xFF, writes
vanish, and `--stats` counts the accesses instead of warning about
unknown devices:

```
WIC/VIC slots (all empty: reads float to 0xFF): slot 0 r=2 w=2 last=0x05; slot 1 ...
```

The behaviour didn't change (the old "unknown device" path also returned
0xFF), only now it is understood. Emulating an actual card is a separate
project: a WIC-1T would need its ID EEPROM contents and the CPM SCC in
HDLC mode.

## 19.2 The IO FPGA registers

The registers IOS touches on the 1760 (CPLD/IO FPGA at 0x68020000; see
`board/cpld.rs` for the full table):

| reg | what IOS does with it | function |
|---|---|---|
| 0x0A | slot 3's control register: bit 0 = reset pulse, plus its device select field (the counterpart of reg 0x18 for slots 0-2) | `wic_slot_reset_pulse` 0x801405BC, `wic_slot_select` 0x80504B8C |
| 0x0C | written once at init: 0 on the 1751/1760 (15 on PM ID 0x1A8); meaning not visible from the code | `iofpga_reg0c_init` 0x80122110 |
| 0x10 | bit 3 = VPN module fitted (IOS then reserves "VPN Module Buffers") | `iomem_size_slots` |
| 0x14 | bits 6/7 = voice card in slot 0/1 | `c17xx_slots_init` 0x8013EF88 |
| 0x18 | bits 0/1 = reset pulse of slots 1/2; device selects for slots 0-2 (the SPI EEPROM selects 0x08/0x20 of chapter 10.2 and the values 0x70, 0xD0... Dynamips doesn't list) | as 0x0A |
| 0x1C | bit 5 = slot 0 reset pulse | `wic_slot_reset_pulse` |
| 0x20 | bits 6/7 read when choosing the FPGA image size | `fpga_firmware_register` |
| 0x48, 0x5C, 0x6C, 0x70, 0x74 | **voice TDM**: routing of voice ports to the MPC860's time slot assigner ("TSA %d <--- port %d/%d"), DSP time-slot programming | `voice_tdm_init` 0x8013E4F4, `tsa_route_voice_port` 0x800DC690, `dsp_timeslot_program` 0x8062826C |

`c17xx_slots_init` (0x8013EF88) ties it together on a 1751/1760: load the
FPGA bitstream (XC2S200, 166 980 bytes on the 1760, chapter 11.6), set the
voice-card bits, initialise the voice TDM, then pulse every slot's reset
at once (regs 0x18, 0x1C, 0x0A).

Without cards or DSPs, plain read/write latches are exactly right for all
of these, which is what the model already did. What changed is the
documentation in the model.

## 19.3 The five machine checks

`--stats` reports five machine checks in every IOS boot. The core now logs
each one (`--log-level warn,exception=debug`):

```
machine check at pc=0xfff10054, data address 0x08080000   ROMMON
machine check at pc=0xfff0fdf8, data address 0x08080000   ROMMON
machine check at pc=0xfff0fe10, data address 0x62000000   ROMMON
machine check at pc=0x81bc804c, data address 0x62000000   IOS
machine check at pc=0x81bc8078, data address 0x62000000   IOS
```

0x08080000 is just past 128 MB of RAM and 0x62000000 just past 32 MB of
flash (CS6 at 0x60000000). Both are **deliberate probes**: no chip select
answers, the bus monitor ends the cycle with TEA, the core takes a machine
check, exactly as on the real board.

On the IOS side the function is `probe_address` (0x81BC7F80): it installs a
temporary machine check handler (vector 0x200), reads the address, and
returns 0 if the read faulted. Platform init uses it to find the end of
the flash; the memory dump command uses it to print `XX` for unreadable
bytes. ROMMON does the same while sizing memory (chapter 3). Nothing to
fix: the five machine checks are the emulator being faithful.
