# 23. A WAN card: the WIC-1T

Until now the four WIC/VIC slots were empty windows (chapter 19.1). This
chapter fits the most common card, the **WIC-1T**, one synchronous serial
port, and cables two emulated routers together through it:

```
c1700emu ... --wic0 wic-1t --serial0 udp:31000:127.0.0.1:31001    # router A
c1700emu ... --wic0 wic-1t --serial0 udp:31001:127.0.0.1:31000    # router B

A# show interfaces Serial0/0
Serial0/0 is up, line protocol is up
  Hardware is PowerQUICC Serial
     DCD=up  DSR=up  DTR=up  RTS=up  CTS=up
A# ping 10.9.9.2
!!!!!
Success rate is 100 percent (5/5), round-trip min/avg/max = 4/4/4 ms
```

Everything below was read out of IOS 12.4(8) with IDA; the function
names are in `tools/ida/ios_124_8_names.py`. Each step is something IOS
needs to see before it goes to the next one.

## 23.1 "Is there a card?"

IOS's card discovery (`wic_slot_object_init`) looks at a slot only if
the platform's **card-present** function says it is occupied. On a 1760
that is `c1760_cards_present`: IO FPGA register **0x10**, bit 1 for slot
0, bit 2 for slot 1, bit 0 for slot 2 and bit 5 for slot 3, plus the
voice-card presence test of chapter 19.1. Our CPLD read 0 there, so IOS
read a card's EEPROM perfectly and then ignored it. Register 0x10 now
has card-present inputs (`cpld::CARD_PRESENT`), set for the fitted
cards.

## 23.2 "What card is it?": the ID EEPROM

Each card has a serial EEPROM on the board's SPI bus. IO FPGA register
0x18 (0x0A for slot 3) holds a device-select field per slot; device 3 of
the slot selects its EEPROM (IOS's table at 0x8268A74C):

| slot | register | field | EEPROM selected | idle |
|---|---|---|---|---|
| 0 | 0x18 | 0x60 | 0x60 | 0x40 |
| 1 | 0x18 | 0x18 | 0x18 | 0x10 |
| 2 | 0x18 | 0x84 | 0x84 | 0x80 |
| 3 | 0x0A | 0x30 | 0x30 | 0x20 |

`slot_eeprom_read` reads 64 big-endian words, each with one SPI transfer
`03 <byte address> 00 00`: the EEPROM's READ command, then two bytes
clocked out. (The SPI engine itself has existed since chapter 10.2; it
just always heard 0xFF.) A card with an old-format EEPROM (version 1 or
2) gives its card type in byte 1; the WIC-1T is **0x02** (IOS's card-name
switch at 0x80502C88: 0x02 "WAN Interface Card - Serial (1T)", 0x12 2T,
0x13 2A/S, ...). The rest of the layout is what `show diag` prints
(`show_diag_wic_slot`), and `wic.rs` fills it in:

```
WIC/VIC Slot 0:
Serial 1T
Hardware revision 1.0      Board revision A0
Serial number     0020041000    Part number    73-2834-07
FRU Part Number   WIC-1T=
Test history      0x00          RMA number     00-00-00
Connector type    WAN Module
EEPROM format version 1
```

## 23.3 The serial port is an SCC

`wic_slot_scc_channels` maps a one-port card in slot 0 to SCC channel 0
and slot 1 to channel 1, and the driver's helpers address the SCC
registers at IMMR + 0xA00 + 32·n and its parameter RAM at
IMMR + 0x3C00 + 256·n: **slot 0's port is SCC1, slot 1's is SCC2**. IOS
calls it "PowerQUICC Serial" and runs it in HDLC mode.

The SCC engine (`board/scc.rs`, MPC860UM chapters 21 and 23) follows the
same pattern as the SPI and the FEC: registers in the MPC860 model, the
engine on the board because it needs memory access.

* **Transmit**: from TBPTR, every frame whose buffers are all ready up to
  the one with L is sent, its buffers closed, SCCE[TXB] set for those with
  I. It runs when software writes TODR[TOD] (transmit on demand), enables
  the transmitter, or issues RESTART TRANSMIT, and at every device sync
  (the hardware polls every 64 clocks). STOP TRANSMIT stops it; GRACEFUL
  STOP TRANSMIT too, setting SCCE[GRA] at once (no frame is ever half
  sent).
* **Receive**: a frame is written to the RxBDs from RBPTR, MRBLR bytes per
  buffer, followed by its CRC: CRC-16/X.25 or CRC-32 as PSMR[CRC] says,
  and included in the last buffer's length, as the hardware does. Address
  recognition through HMASK/HADDR1-4, SCCE[BSY] when no buffer is free,
  SCCE[RXF] every RFTHR frames.
* **Interrupts**: SCCE & SCCM sets the SCC's CIPR bit (SCC1 = bit 1).
* **Local loopback** (GSMR_L[DIAG] = 01): sent frames come back to the
  same receiver.

Frames leave through the port's link without flags or CRC; the CRC is
generated again on reception.

## 23.4 Modem signals and the cable: AutoInstall over a floating line

With the SCC working, the first boot with a WIC-1T stalled for six
minutes after the setup dialog, and the port sent hundreds of frames:
SLARP requests (`8f 00 80 35`) and BOOTP broadcasts (`8f 00 08 00`). That
is IOS **AutoInstall over the serial line**, which it only tries on a
serial port whose line is up. Our line looked up because the modem
signals are active-low port pins that read 0. (The six minutes were the
session driver: IOS asks "Would you like to terminate autoinstall?",
which `tools/ios_session.py` now answers.)

Where the signals are (`serial_dcd`, `serial_dsr`, `serial_cts`, ...,
DTE wiring):

| signal | port 0 (SCC1) | port 1 (SCC2) |
|---|---|---|
| DCD (in) | PCDAT 0x20 (PC10, CD1) | PCDAT 0x80 |
| CTS (in) | PCDAT 0x10 (PC11, CTS1) | PCDAT 0x40 |
| DSR (in) | PBDAT 0x80 | PBDAT 0x20 |
| DTR (out) | PBDAT 0x800 | PCDAT 0x08 |
| RTS (out) | PBDAT 0x1000 | PBDAT 0x2000 |

All active low. The card also has registers in its slot window
(`wic_serial_modem_init`): at **+4**, bits 0-1 select one of four
cable-ID pins, read back in bit 0x20 (`wic_cable_id_read`), and bits 2-4
strobe the line transceivers' modes; at **+8**, a "cable changed" latch
and two control bits. The 4-bit cable ID (`wic_cable_type`): bit 0 set
means a DCE cable; 2 is V.24, 4 V.35, 6 RS-449, 8 X.21, 14 EIA-530, 16
530A; **0x01 means "No serial cable attached"**. Finally `show
controllers` samples the port A clock pins and reports clocks when they
toggle.

The MPC860 model now has board-driven input pins (`PortInputs`): reads
of PCDAT and PBDAT see the board's levels in the modem-signal bits, and
the clock bits of PADAT toggle. Without `--serial0` the port has no
cable: signals high (deasserted), cable ID 0x01. With it: a V.35 DTE
cable, DCD/CTS/DSR asserted, clocks running.

```
Router# show controllers serial 0/0          Router# show controllers serial 0/0
No serial cable attached                     DTE V.35 TX and RX clocks detected.
     DCD=down DSR=down ... CTS=down               DCD=up DSR=up ... CTS=up
```

## 23.5 Cabling two routers, and a line that looked looped

`--serial0 udp:LPORT:RHOST:RPORT` carries each HDLC frame in a UDP
datagram, like the Ethernet tunnels of chapter 18 (the same `HostLink`;
its receiver thread also wakes an idle emulator). Two routers configure
`Serial0/0` with Cisco HDLC (the default encapsulation), exchange SLARP
keepalives every 10 seconds, and the line protocol comes up.

The first runs sometimes said `line protocol is up (looped)` on one
side. IOS declares a serial line looped when the keepalive it receives
carries the same "magic" value as the last one it sent
(`hdlc_keepalive_check`; the value is `keepalive_magic`: a counter plus
the uptime in ms plus a constant). Two emulated routers booted from the
same images run **cycle for cycle alike**: the emulator is deterministic.
So their keepalive number n leaves at the same uptime with the same
magic, and to IOS each router seems to hear itself. Different MAC
addresses and serial numbers don't change the magic, and neither does
starting one router 7 seconds later (keepalives follow uptime). Real
routers never run in lockstep. In the test the two routers simply use
different keepalive periods (10 and 7 s), as real ones may.

## 23.6 Tests and limits

* `board::wic::tests`: the EEPROM answers IOS's READ commands, the select
  registers pick the slot, the cable ID reads through the mux;
  `board::scc::tests`: the CRC-16/X.25 check value.
* `c1700/tests/scc_loopback.rs`: a synthetic ROM programs SCC1 with one
  TxBD and one RxBD in the dual-port RAM, in local loopback, and asks for
  transmission on demand; the frame must come back followed by its
  CRC-16, buffers closed, SCCE = TXB | RXF, with both CPU engines.
  Removing the loopback path makes it fail.
* `tools/tests/serial_test.sh [--cpu interp|jit]`: two routers, line
  protocol up on both, pings both ways. Passes with both engines:

  ```
  == router A
  Serial0/0 is up, line protocol is up
       DCD=up  DSR=up  DTR=up  RTS=up  CTS=up
  Success rate is 100 percent (5/5), round-trip min/avg/max = 4/4/4 ms
  DTE V.35 TX and RX clocks detected.
  ```

  and `--stats` counts the frames: `SCC1 (HDLC, UDP 31795 ->
  127.0.0.1:31796): 34 frames sent, 34 received`.

Not modelled: the cable-change interrupt (the cabling is fixed when the
emulator starts), and the line speed: frames go out as soon as they are
ready, whatever `clock rate` says. Pacing them would need the DCE end's
baud-rate generator (SICR, BRGC, SCCR) and, on the DTE end, a clock the
UDP tunnel doesn't carry; Dynamips doesn't pace its serial links either.
Snapshots save the SCC and card state (format 3).

## 23.7 The WIC-2T and DCE cables

Two more pieces fell out of the same IOS functions:

* **The WIC-2T** (card type 0x12, "Serial 2T (12in1)"): two ports. The
  second port's registers are at +0x14/+0x18 of the window, and
  `wic_slot_scc_channels` gives two-port cards SCC channels [0, 3] in slot
  0 and [1, 2] in slot 1: slot 0's second port is **SCC4**, slot 1's
  **SCC3**. IOS numbers the ports Serial0/0, Serial0/1... in slot order.
  The modem pins of channels 2 and 3 are in the same functions.
* **DCE cables**: the cable ID's bit 0. IOS then reads DCD/DSR and CTS on
  the pins a DTE drives as DTR and RTS (port 0: PBDAT 0x800 and 0x1000),
  drives the clock itself, and `show controllers` prints the clock rate
  instead of sampling clock pins. An uncabled port drives both sets of
  inputs high: IOS assumes DTE wiring until it has read the cable ID (the
  first version left the DTE pins at 0 and an unconnected 2T port came up
  for a moment).

```
c1700emu ... --wic0 wic-2t --serial0 udp:33000:127.0.0.1:33001,dce --serial0-1 udp:33002:127.0.0.1:33003
```

`tools/tests/serial_2t_test.sh` cables two routers twice, a DCE end on
each side (with `clock rate 64000`): both lines up, pings on both, and

```
Router# show controllers serial 0/0        Router# show controllers serial 0/1
DCE V.35, clock rate 64000                 DTE V.35 TX and RX clocks detected.
```
