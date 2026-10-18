# TODO

Open work on the C1700 emulator, roughly by priority inside each group.
Chapter references are to `docs/`.

## Emulator: hardware and features

- [x] 1. Network bridge: `--net tap:NAME` and `--net udp:...`, `--mac`
      (chapter 18). Two emulated routers ping each other over UDP.
  - [x] 1a. TAP tested end to end (pings both ways), without sudo, in a
        user + network namespace: `tools/tests/tap_test.sh` (chapter 18.4).
- [x] 2. WIC/VIC slots understood and modelled as empty slots (chapter 19.1).
  - [x] 2a. A real card: the WIC-1T (`--wic0 wic-1t`, `--serial0 udp:...`):
        card-present bits, ID EEPROM over SPI, SCC in HDLC mode, modem
        signals and cable ID; two routers ping over Serial0/0
        (chapter 23, tools/tests/serial_test.sh).
- [x] 3. IO FPGA registers decoded (chapter 19.2); latches are right
      without cards.
- [x] 4. The 5 boot-time machine checks: deliberate probes past the end
      of RAM and flash, faithful (chapter 19.3).
- [x] 5. `--aux-tcp`: IOS's AUX line (`aux 0`) gives a second exec session
      (chapter 7).
- [x] 6. Snapshots (`Ctrl-] s`, `--restore`; IOS at `Router#` in 0.3 s)
      and ELF core dumps for GDB (`Ctrl-] d`), chapter 20.
- [x] 7. Warp: the "silent console" was IOS's exec-timeout logout (needs a
      bare RETURN); `--idle warp` is back; a snapshot/idle-detection bug
      found on the way is fixed (chapter 21, tools/tests/warp_test.py).

## JIT (chapter 17.18)

- [x] 8. Flash read directly in read-array mode (`host_page` + a cheap
      `host_page_epoch` that only empties the fast tables when a command
      leaves read-array mode), +17%, chapter 17.15.
- [x] 9. Cheaper compare-and-branch: a branch tests the comparison's
      result instead of re-reading the CR bit (chapter 17.13).
- [x] 10. Guest registers stay in host registers within a guest block
      (TCG-style dirty tracking, stored at block end, exits and helper
      calls); whole-region allocation was tried and is slower (chapter 17.13).
- [x] 11. Regions jump straight into each other (chaining: links filled
      in by the dispatcher, tail calls, one-entry inline cache for
      `blr`/`bctr`), +17%, chapter 17.14.
- [x] 12. `--cpu jit-check`: every region run verified against the interpreter
      (record + replay), chapter 17.12; all JIT tests and the ROMMON boot
      run in check mode too.
- [x] 13. The interpreter was slower with the JIT built in (7% by then):
      LLVM stopped inlining `Board::fetch` into the interpreter loop once
      the JIT called it too; now `#[inline(always)]`, interpreter 32.0 →
      35.8 MIPS (chapter 17.9).

- [x] 16. Found on the way: device syncs every 256 instructions made the
      JIT interpret the end of every batch and pay per-batch bookkeeping;
      devices are now synced at their next event (at most every 4096),
      JIT 415 → 650 MIPS, IOS to `Router>` (--idle off) in 21.3 s,
      chapter 17.16.
- [x] 17. Found on the way: host → router pings took 7-9 ms. Input threads
      now wake the idle sleep, the idle stop no longer ignores a pending
      interrupt, busy work isn't throttled by the idle slack, and ROMMON's
      key-wait sleep no longer runs under IOS: 0.6-0.7 ms (chapter 18.5).
- [x] 18. Found on the way: the JIT compiled on the spot, so when a lot of
      code got hot at once (IOS's IP stack under the first pings) the
      router stalled for hundreds of ms. Regions are now compiled by a
      background thread while the interpreter goes on: first pings 0.6 ms
      instead of up to 626 ms, IOS to `Router>` (--idle off) 18.8 s
      (chapter 17.17).

## IDA and docs

- [x] 14. 0x43a0 is ROMMON's stack, holding a copy of its checksum
      routine it runs from RAM; ROM routines named (bootdev_checksum,
      bootdev_range, bootdev_read, copy16, cache maintenance), chapter 22.
- [x] 15. Chapter 17's worked example also shows the loop as a region
      (jump table, step counter, budget check).
