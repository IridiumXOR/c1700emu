# 15. Profiling, and idle detection

Chapter 14 listed ways to make the emulator faster. This chapter measures
first: where the emulated router spends its instructions, and where the
emulator spends its host time. Then it implements the first result: **idle
detection**, which skips IOS's idle loop instead of emulating it.

## 15.1 Tools

* **Guest profile**, `--profile`: besides the exact per-instruction counts
  for the boot ROM, the PC is sampled every 997 instructions (a prime, so
  sampling can't lock onto a loop's period) over the whole address space.
  At exit, the hottest 4 KB pages and addresses are printed for the whole
  run and for the **window** since the last `Ctrl-] p`.
  `--profile-dump FILE` writes every sample (`address whole window`) for
  offline analysis; aggregated by IDA function (chapter 2.5), it shows which
  routines matter.
* **Host profile**: `perf record -F 499 -- c1700emu …`, then
  `perf report --sort symbol`. The release build keeps debug info.
* `tools/ios_session.py` directives `profile-reset` (sends `Ctrl-] p`) and
  `sleep N` script "boot, then profile 60 s of idle".

## 15.2 Where the guest's instructions go

Boot to `Router>`, IOS 12.4(8) on a 1760, 7.3 billion instructions
(dump aggregated by function):

| share | what |
|---|---|
| **37.6%** | IOS's **self-decompressor** (inflate): 0x80008000-0x8000BFFF while it runs, plus a part relocated to the top of RAM (0x87Fxxxxx) |
| **~35%** | the **scheduler idle loop**: `scheduler_find_next_process` 13.7%, `timer_expired` 8.3%, `mgd_timer_check` 5.0% and smaller helpers |
| **12.7%** | the **boot ROM**, mostly clearing all 128 MB one byte at a time at power-on |
| ~2.5% | exception handlers in low memory |
| rest | IOS initialisation proper; the FPGA bitstream load is 0.6% |

A router idle at `Router>` for 60 s: 1.27 billion instructions, **all** in the
scheduler loop. The samples spread evenly over ~330 addresses, one pass of
the loop, each instruction executed once per pass.

## 15.3 Where the emulator's host time goes

`perf` over a whole session, by function (the instruction cycle is inlined
into `Cpu::step`):

| host time | function | meaning |
|---|---|---|
| 51.5% | `Cpu::step` | interrupt check, ITLB, decode cache, execute, tick |
| **18.0%** | `Board::read32` | mostly **instruction fetch** through the bus decoder |
| **11.3%** | `Board::region` | the bus page-table lookup behind every fetch and access |
| 6.7% | `Machine::run` | per-instruction hooks (breakpoints, fast-forward, profile, watch, idle) |
| ~8% | load/store helpers | data accesses |

Roughly **30% of host time is spent finding memory**, mostly to fetch
instructions. That's the first target of the interpreter work (a direct
pointer to the current code page). The core alone also lost ~14% to the
MMU checks even with translation off (chapter 5).

## 15.4 How IOS idles

IOS doesn't use the MPC860's low-power modes. No `mtmsr` sets `MSR[POW]`
(all 105 were checked in IDA), and PLPRCR is written only once, by the boot
ROM. When no process is ready, `scheduler_find_next_process`
(0x80617618) loops:

* the **outer loop** (head 0x80617648) checks the managed timers
  (`timer_expired` compares a deadline with `get_system_time`, the 64-bit
  clock the PIT interrupt keeps in RAM);
* for queues with blocked processes, it calls each one's **wake-up
  predicate** (`bctrl` at 0x80617ebc on 0xC0(process), argument 0xC4).

Nothing changes until an interrupt does: the 4 ms PIT tick advances the
clock, a frame arrives, a key is pressed. Every pass in between is identical.

## 15.5 The detector

The design follows Dynamips' **idle PC**, adapted to our clock:

1. **Anchor**: an address executed in every idle pass. `--idle-pc auto`
   (the default) knows the 12.4(8) anchor and only uses it after checking
   that the four instruction words there are the expected ones, so another
   image, or memory before IOS is decompressed, never triggers it.
   `--idle-pc ADDR` sets one by hand, unchecked.
2. **Idle = the same pass repeating.** The distances (in instructions)
   between anchor hits must repeat with a period of up to 8 hits, with
   interrupts enabled and a pass shorter than 20 000 instructions, three
   times in a row. A process that runs, or an interrupt arriving mid-pass,
   breaks the pattern.
3. **Skip**: jump virtual time to one timebase tick before the next
   decrementer or PIT interrupt, using the same `skip_timebase` as the ROM
   delay-loop fast-forward. The skip is never taken when an interrupt is
   already pending, and never crosses a timer event, so interrupts arrive at
   the same virtual time as without skipping.
4. **Pacing** (`--idle realtime`, the default; `--idle off` disables it):
   virtual time may not run ahead of the wall clock, so the emulator sleeps
   instead of spinning. An idle router uses almost no CPU and its clock
   follows real time.

   A **warp** mode (no wall-clock limit) was tried and removed. After a
   minute of virtual time racing ahead (5.6 virtual hours in one run), IOS
   stopped answering the console entirely: no echo. *Solved later
   (chapter 21): IOS had logged the console out after its 10-minute
   exec-timeout, and a logged-out line only wakes up on a bare RETURN.
   `--idle warp` is back.* Warp would have been nearly useless anyway: during
   boot, virtual time is *behind* the wall clock, so realtime mode already
   skips as fast as warp would.

Dynamips can simply sleep, because its clock is the host's. Ours counts
instructions, so the *skip* is what saves the work. Pacing only decides what
happens with the wall-clock time saved.

**A wrong anchor first.** My first anchor, 0x80617e3c, came from the idle
window. It sits in a queue-check loop, and during boot IOS idles mostly in
the *outer* timer loop, which doesn't go through it: only 7.2 s of virtual
time was skipped during boot. The boot-time dump showed a second uniform
loop (~15 million passes, 0x80617648 … 0x80617988 `bne 0x80617648`). Its
head is reached in every pass, boot or idle, and that is the anchor now.
The detector's design made the wrong anchor harmless: it only lost speed.

## 15.6 Results

Same scenario (boot, `show clock`, 30 s idle, `show clock`, configure,
`ping 10.0.2.2`), IOS 12.4(8), 1760, 128 MB:

| | no idle detection | idle detection (defaults: `auto`, `realtime`) |
|---|---|---|
| `Router>` reached | 318 s | **206-216 s** (-33%) |
| host CPU, idle router | 100% of a core | **~0-1 s per 5 s** |
| ping 10.0.2.2 | `.!!!!` | `.!!!!` |
| IOS `show processes cpu` | | `0%/0%` |
| virtual time skipped (whole session) | | 199.5 s in 50 320 skips |

After a slow boot the router's clock is behind the wall clock; in realtime
mode it then catches up (`show clock` moved 71-80 s during a 30 s wait) and
stays in step afterwards. With `--idle-pc auto` the log shows `idle anchor
0x80617648 recognised: IOS 12.4(8) c1700-adventerprisek9-mz` once IOS runs.

*Bug found on the way:* the first version decided "pace to the wall clock?"
once, when the run started. In auto mode the anchor is only recognised
later, so pacing never switched on: virtual time raced, which is how warp
mode's console problem showed up. The decision is now taken on every slice.

## 15.7 What the profile says about the rest

With the idle loop gone, the boot is dominated by:

1. **the self-decompressor** (~2.75 billion instructions of inflate loops):
   pure computation, the case where a faster interpreter or a JIT pays most;
2. **the ROM's byte-by-byte RAM clear** (~0.9 billion instructions): a
   cheap, ROM-specific accelerator in the style of `fastforward.rs`;
3. **IOS initialisation**.

And on the host side, ~30% of the time finding memory, mostly for
instruction fetch. These numbers are the input for the JIT decision
(chapter 14).

## 15.8 Steps 1 and 2: ROM accelerator and interpreter fast paths

The two items this profile pointed at were done next, and are described in
detail in **chapter 16**:

1. the ROM's byte-by-byte memory clear is done as one bulk fill, with a test
   proving the result identical to running the loop (ROMMON boot with
   128 MB: 25.7 → 0.2 s);
2. the interpreter's fixed costs were cut: bus fast paths, hook pages, an
   address-indexed decode cache and a batch loop. +47% on the benchmark,
   32.5 million executed instructions per second (the 40.3 "MIPS" first
   written here counted the instructions skipped by the memory clear; see
   chapter 16.1).

**Result for IOS** (same scenario as 15.6):

| | before idle detection | idle detection | + steps 1 and 2 |
|---|---|---|---|
| `Router>` reached | 318 s | 216 s | **153 s** |
| ping | `.!!!!` | `.!!!!` | `.!!!!` |
| 30 s idle on the wall clock | | 80 s virtual (catching up) | 30.1 s virtual |

With the faster boot, virtual time catches up with the wall clock during
the boot already, so the idle router keeps exact real time from the
prompt on.
