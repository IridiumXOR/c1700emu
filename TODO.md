# TODO

Open work on the C1700 emulator, roughly by priority inside each group.
Chapter references are to `docs/`.

## Emulator: hardware and features

- [x] 1. Network bridge: `--net tap:NAME` and `--net udp:...`, `--mac`
      (chapter 18). Two emulated routers ping each other over UDP.
  - [x] 1a. TAP tested end to end (pings both ways), without sudo, in a
        user + network namespace: `tools/tests/tap_test.sh` (chapter 18.4).
- [x] 2. WIC/VIC slots understood and modelled as empty slots (chapter 19.1).
  - [ ] 2a. Emulate a real card, e.g. WIC-1T: ID EEPROM contents over SPI
        plus the CPM SCC in HDLC mode (serial interfaces).
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

## JIT (chapter 17.12)

- [ ] 8. Read flash directly (ROMMON's copy of the 21 MB image is ~10% of
      the benchmark); needs cheap invalidation when the flash leaves
      read-array mode.
- [ ] 9. Cheaper compare-and-branch: keep the comparison result for the
      branch instead of re-reading the CR bit.
- [ ] 10. Keep guest registers in host registers instead of writing every
      change through to the `Cpu` struct.
- [ ] 11. Jump directly between regions instead of through the dispatcher.
- [ ] 12. `--cpu jit-check`: run every region on both engines and compare,
      so a full IOS boot verifies the JIT.
- [ ] 13. The interpreter is ~2% slower with the JIT built in, even unused
      (mostly code layout; 2% unexplained, chapter 17.9).

## IDA and docs

- [ ] 14. Identify and annotate the low-memory structure at physical 0x43a0
      (a halfword variable right before code) and the ROM routines at
      0xFFF23C34 / 0xFFF23C88.
- [x] 15. Chapter 17's worked example also shows the loop as a region
      (jump table, step counter, budget check).
