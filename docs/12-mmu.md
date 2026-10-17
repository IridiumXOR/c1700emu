# 12. The MPC8xx MMU

Until this chapter the emulator ignored address translation: when software
set `MSR[IR]`/`MSR[DR]` it logged a warning and kept using the effective
address as the physical one. ROMMON never turns translation on; IOS does,
and for a long time the identity shortcut was enough to boot it (Dynamips
takes the same shortcut). This chapter covers the evidence that the shortcut
had stopped working, how the 8xx MMU works, how IOS programs it, and how it
is emulated (`emu/ppc8xx/src/mmu.rs`).

## 12.1 Why now (and a hypothesis that didn't survive)

With FastEthernet up, IOS sent ARP requests and our virtual host answered,
but the ping failed. The FEC wrote every reply into IOS's receive buffers
and IOS acknowledged each interrupt, yet IOS's own buffer dump showed the
packets it held were **all zeros**:

```
Router# show buffers input-interface FastEthernet0/0 packet
  linktype 0 (None), enctype 1 (ARPA), encsize 14, rxtype 45
  datagramstart 0x713ECAA, datagramsize 60, maximum size 260
       0:                            00000000              ....
       4: 00000000 00000000 00000000 00000000  ................
```

My first explanation: IOS reads the buffers through virtual addresses that
its MMU maps elsewhere, and our identity mapping read memory the DMA never
wrote. That was a reason to implement the MMU, which you had asked for
anyway. The MMU now works, and its TLB log **disproves** the hypothesis:
I/O memory is mapped one-to-one (0x07000000 → 0x07000000), so identity
mapping gave IOS exactly the same bytes. The receive problem is elsewhere
(chapter 10). Lesson: a plausible mechanism is not evidence; the TLB log
was.

## 12.2 How the 8xx MMU works

The 8xx MMU is nothing like the classic PowerPC one: no segment registers,
no BATs, no hardware table walk.

* **Two TLBs**, instruction and data, 32 entries each, fully associative.
  Each entry maps one page of 4 KB, 16 KB, 512 KB or 8 MB, tagged with an
  address-space ID (unless marked shared).
* **Software refill.** An access whose page is not in the TLB raises an
  *ITLB miss* (vector 0x1100) or *DTLB miss* (0x1200). Before taking it,
  the hardware stores the missing page number in `MI_EPN`/`MD_EPN`.
* **Table-walk assist.** The handler walks a two-level page table with two
  "magic" registers. Reading `M_TWB` returns the *address of the
  level-one descriptor* (table base + EA bits 0-9 × 4). After the descriptor
  is written to `MD_TWC`, reading `MD_TWC` returns the *address of the
  level-two descriptor* (L2 base + EA bits 10-19 × 4, where bits inside a
  large page don't count: no index for 8 MB pages, EA[10-12] for 512 KB; see
  §12.5).
* **Loading.** Writing the level-two descriptor (the PTE) to `MI_RPN`/
  `MD_RPN` loads the TLB entry at `Mx_CTR[INDX]`; INDX then counts down,
  skipping entries 28-31 when `RSV4` "locks" them.
* **Faults.** If the segment or page valid bit was 0, the entry is still
  loaded and the retried access raises an *ITLB/DTLB error* (0x1300/
  0x1400). The same happens on protection violations, on writes to pages
  whose changed bit C is 0 (software dirty-bit tracking) and on instruction
  fetches from guarded pages. SRR1 (instruction side) or DSISR/DAR (data
  side) give the cause.
* **Protection.** It combines the page's PP bits (basic or extended
  encoding; per 1 KB sub-page in "mode 2") with one of 16 access protection
  groups in `Mx_AP`. In the default mode a group can force "treat everyone
  as supervisor/user" or swap them; in domain-manager mode it can deny
  everything or allow everything.

The manual's own reload handler (Figure 8-23) is nine instructions:

```
mtspr M_TW, r1      ; save r1
mfspr r1, M_TWB     ; r1 = address of the level-one descriptor
lwz   r1, 0(r1)
mtspr MD_TWC, r1    ; level-one descriptor in
mfspr r1, MD_TWC    ; r1 = address of the level-two descriptor
lwz   r1, 0(r1)
mtspr MD_RPN, r1    ; load the TLB entry
mfspr r1, M_TW
rfi
```

## 12.3 How IOS uses it

Disassembling the code that wrote the MMU registers (`tools/iosdis.py`,
0x80130780) shows IOS preloading **locked TLB entries** with exactly that
sequence, one per 8 MB region:

```
mtspr M_TWB, base         ; level-one table at 0x038B0000
MD_CTR[INDX] = counter++  ; a reserved slot
mtspr MD_EPN, ea | EV
mfspr r9, M_TWB ; lwz r9,0(r9) ; mtspr MD_TWC, r9
mfspr r9, MD_TWC ; lwz r9,0(r9) ; mtspr MD_RPN, r9
```

The regions it pins: 0x07000000 and 0x07800000 (I/O memory, the 16 MB at the
top of the 128 MB), 0x50000000, the flash at 0x60000000-0x64000000, the I/O
bus 0x68000000, IOS itself at 0x80000000-0x86800000 (data and
instructions), the IMMR window 0xFF000000 and the boot ROM 0xFFF00000.

Our first, identity-mapped emulator logged these writes, and decoding them
gave nonsense: every entry seemed to map to the same physical page
0x90600000. The reason is the table-walk assist. We returned the *stored*
value when IOS read `M_TWB`/`MD_TWC`, so IOS loaded descriptors from the
wrong addresses. Since translation was ignored anyway, nothing visibly broke.

## 12.4 Implementation

`emu/ppc8xx/src/mmu.rs` (core crate, so it is tested without the board):

| Piece | Behaviour |
|-------|-----------|
| `Tlb` ×2 | 32 decoded entries (EPN, ASID, page mask, RPN, raw L1 attributes and PTE), `Mx_CTR/EPN/TWC/RPN/AP`, debug CAM/RAM read-out |
| `translate` | match (page size, ASID/shared, PPCS user/supervisor bits or 1 KB sub-page validity) → valid bits → guarded fetch → PP + group protection → C bit → physical address |
| on a miss | `Mx_EPN` = EPN \| EV \| CASID, `Mx_TWC` = defaults, counters for `--stats` |
| `read_spr` | `M_TWB` and `MD_TWC` hardware assists (TWAM = 1 and 0) |
| `write_spr` | `Mx_RPN` loads an entry: overlap invalidation, INDX decrement modulo 32/28 |
| `tlbie` / `tlbia` | invalidate one page in both TLBs / everything except locked entries; tlbia clears the replacement counters |
| `FastCache` | 256-entry direct-mapped memo of 4 KB translations with precomputed permissions, flushed on any TLB/CTR/AP/CASID write |

In the core (`lib.rs`, `execute.rs`): fetches translate when `MSR[IR]=1`,
and the load/store funnel (`load8/16/32`, `store8/16/32`, so also the
string, `lwarx`/`stwcx.` and `dcbz` paths) translates when `MSR[DR]=1`;
`icbi`, `dcbst`, `dcbf` and `dcbi` translate their address too (§8.9).
There are four new exceptions with the SRR1/DSISR/DAR contents of Tables
6-13..6-16. GDB memory accesses and the `--stats` disassembly go through the
TLBs (`Machine::debugger_address`, no side effects).

Tests:

* 8 unit tests in `mmu.rs`: assists, 8 MB pages, invalid page → error not
  miss, PP encodings, C bit, group modes, ASID/shared/PPCS, guarded fetch,
  locked entries vs `tlbia`/`tlbie`, overlap replacement;
* 3 end-to-end tests in `ppc8xx/tests/mmu.rs`: real code takes a DTLB miss,
  the manual's Figure 8-23 handler refills the TLB and the load is retried;
  a store to a read-only page raises a DTLB error with the right DSISR/DAR;
  an ITLB miss is serviced by the Figure 8-24 handler.

## 12.5 What IOS taught us

**IOS pins everything and has no miss handlers.** All 44 TLB loads in a
boot are IOS's pinned 8 MB/512 KB/16 KB entries (15 instruction, 29 data).
IOS's handler at 0x1100 is a generic stub that jumps through a table, and
the ITLB-miss slot is **0**. A single ITLB miss is therefore fatal: the
first MMU run spun 38.9 million times through "miss → jump to 0 → miss".
Data accesses never miss either (0 DTLB misses per boot). The TLB
contents are the whole memory map:

| virtual | → physical | page | attributes |
|---------|-----------|------|------------|
| 0x80000000–0x86FFFFFF | 0x00000000… | 8 MB | IOS in DRAM; text pages read-only |
| 0x07000000–0x07FFFFFF | same | 8 MB | I/O memory, cache-inhibited |
| 0x60000000–0x63FFFFFF, 0x64000000 | same | 8 MB, 512 KB | flash |
| 0x68000000 | same | 8 MB | I/O bus (guarded) |
| 0xFF000000 | same | 16 KB | IMMR |
| 0xFFF00000 | same | 512 KB | boot ROM (IOS calls ROMMON) |
| 0x50000000 | same | 512 KB | ? |

**The `MD_TWC` assist for large pages.** §8.7 says that for 512 KB and
8 MB pages "the level-two base address is used directly". Implemented
literally, IOS's ROM entry got PTE 0, because 0xFFF00000 is the only large
page whose EA bits 10-19 aren't zero (they are 0x300). The first IOS
instruction fetched from the ROM then missed, which was fatal as described
above. Read with Table 8-1 (one level-two entry per page), the hardware
returns `L2 base | EA[10-19]<<2`, but the index bits *inside* the page
don't count. For 8 MB pages that leaves no index (the "used directly"
case); for 512 KB pages EA[10-12] still pick one of the eight pages of the
4 MB segment. With that, the ROM PTE is `0xFFF00889` and IOS boots.

**Cost.** Setup dialog after 294 s, against 269 s without translation
(+9%), thanks to the per-page fast path.
