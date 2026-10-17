# 13. IOS networking: the receive path, IDMA and the first ping

IOS sent frames from the first boot on: DHCP discovers, CDP, keepalives,
ARP requests. But it never *received* anything usable: `ping 10.0.2.2`
failed and the ARP entry stayed "Incomplete". Finding out why took four
fixes in four different places, and two wrong explanations along the way.
This chapter keeps both, because ruling out a hypothesis with evidence is
as much part of the method as finding the bug.

The end result:

```
Router# ping 10.0.2.2
Sending 5, 100-byte ICMP Echos to 10.0.2.2, timeout is 2 seconds:
.!!!!
Success rate is 80 percent (4/5), round-trip min/avg/max = 1/1/1 ms
Router# show arp
Internet  10.0.2.2                0   5255.0a00.0202  ARPA   FastEthernet0/0
```

(The first echo is lost while ARP resolves, as on a real router.)

## 13.1 Symptom and first evidence

The FEC debug log (`--log-level fec=debug`) showed each ARP reply written
into IOS's receive ring (`receive: 64 bytes into RxBD 0x7117720 (buffer
0x7042360)`) and IOS acknowledging every receive interrupt (IEVENT
cleared, R_DES_ACTIVE re-armed). So descriptors and interrupts worked.
Dynamips' FEC receive code does the same as ours (status bits, length + 4,
E cleared last), so the descriptor protocol wasn't the problem either.

IOS itself reported the anomaly:

```
FastEthernet0/0: 0 packets input ... Input queue: 10/75/0/0
Router# show buffers input-interface FastEthernet0/0 packet
  linktype 0 (None), enctype 1 (ARPA), encsize 14, rxtype 45
  datagramstart 0x713ECAA, datagramsize 60
       0: 00000000 00000000 ...                (all zeros)
```

Exactly 10 replies had been delivered and 10 packets sat in the queue,
never classified, their contents zero.

## 13.2 Wrong explanation #1: the MMU

The first theory: IOS reads the buffers through virtual addresses its MMU
maps elsewhere, so our identity mapping read memory the DMA never wrote.
It was plausible and it led to implementing the MMU (chapter 12). But the
MMU's own TLB log disproved it: IOS maps its I/O memory one-to-one
(0x07000000 → 0x07000000). With the real MMU the ping still failed, as the
TLB log predicted.

## 13.3 The watch: who reads the frame?

To stop guessing, the emulator got a watchpoint: `--watch START:LEN` logs
every bus access to a physical range with the PC of the instruction, or
`DMA`. Watching the receive buffers:

```
write 0x07042360 = 0x02 ... (64 bytes)                   DMA  (our FEC)
read  0x0704236c = 0x0806 (2 bytes) pc=0x805d8d24        ethertype
read  0x07042360 = 0x02170000 (4 bytes) pc=0x805d8d90    destination MAC
read  0x0704236c = 0x0806 (2 bytes) pc=0x80eca5c0
```

and then nothing. IOS's driver classifies the frame in place (0x805d8d24 is
the Ethernet type switch: 0x0800 → IP, 0x8137 → IPX, …), then the packet
moves to a buffer at 0x713ECAA, **without any CPU instruction reading the
frame**. The copy must be done by hardware.

## 13.4 The IDMA copy engine

The MPC860's CPM has two *IDMA* channels (§19.3), general-purpose DMA run
by the communication processor. IOS's platform init (`idma1_init`,
0x80130368, named in the IDA database) sets IDMA1 up as a memory-to-memory
engine:

* IBASE (descriptor table; IMMR-relative 0x3440 like the SPI tables, i.e.
  DPRAM offset 0x1440), `DCMR = 0` (memory → memory);
* CPCR **INIT IDMA** on channel 1;
* RCCR DREQ0 mode, CIMR |= IDMA1, `IDMR1 = DONE`.

A transfer is requested by the DREQ0 pin; there is no internal request
mechanism. The board must keep DREQ0 asserted, so software starts a copy by
setting **PCSO[DREQ0]**. The completion handler (`idma1_done_isr`,
0x80130010) clears IDSR1, the CPM interrupt and PCSO[DREQ0]. The stats
confirmed it: runs that received packets had exactly one extra PCSO
read-modify-write. IOS started the first copy, it never finished, and every
later frame queued behind it.

Emulation (`board/idma.rs`): while PCSO[DREQx] is set (checked after every
IMMR write), process the valid descriptors from IBPTR. Each 16-byte BD holds
status (V, W, I, L, CM), function codes, length, source and destination.
The copy uses physical addresses (DMA bypasses the MMU). Then clear V
(unless CM), set IDSR[AD]/[DONE], wrap at W, and flag OB when no valid BD
is left. `IDSR & IDMR` feeds CIPR bit 10/11 into the CPM interrupt
controller, and CPCR INIT/STOP IDMA are handled.

First run with it:

```
IDMA1: copy 60 bytes 0x07042360 -> 0x0713ecaa
```

That is the receive buffer copied to exactly the address IOS's dump had
shown as the empty packet. ARP resolved at once (`5255.0a00.0202`), and IOS
sent its echo requests.

## 13.5 Wrong explanation #2 avoided: it was our virtual host

IOS sent five echo requests and got no reply. Before suspecting IOS again,
a unit test fed our virtual network a hand-built, well-formed echo request:
no reply, not even an ARP request. Reading smoltcp 0.14's source showed the
echo-reply code behind `#[cfg(feature = "auto-icmp-echo-reply")]`. We build
smoltcp with `default-features = false`, so the feature was off, and
chapter 8's "answers ARP and ping" had never been true (ROMMON only uses
ARP and TFTP). With the feature enabled, the unit test
(`net::tests::virtual_host_answers_ping`) and IOS's ping both pass.

## 13.6 The four fixes, in the order found

| # | Where | Fix | Evidence |
|---|-------|-----|----------|
| 1 | CPM SPI (chapter 10) | BD offsets decoded modulo the 8 KB DPRAM | 5 544 SPI starts / 0 transfers → 268 / 268 |
| 2 | Core (chapter 12) | MMU, so IOS runs with real translation | IOS boots with translation, 0 TLB misses |
| 3 | CPM IDMA | memory-to-memory IDMA, DREQ via PCSO | IOS's empty packet → `IDMA1: copy 60 bytes`; ARP resolves |
| 4 | Virtual network | smoltcp `auto-icmp-echo-reply` | unit test; `.!!!!` |

## 13.7 Tools that made it possible

* `--watch START:LEN`: bus watchpoint with PC or DMA;
* `fec=debug`, `idma=debug`, `c1700emu::net=debug` log targets;
* IOS's own diagnostics: `show buffers … packet`, `show interfaces …
  switching`, `debug arp`, `show ip traffic`;
* `tools/ios_session.py` with a command file to replay a scenario
  unattended; IDA (MCP) with `tools/ida/ios_124_8_names.py`.
