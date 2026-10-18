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

## JIT (chapter 17.14)

- [ ] 8. Read flash directly (ROMMON's copy of the 21 MB image is ~10% of
      the benchmark); needs cheap invalidation when the flash leaves
      read-array mode.
- [ ] 9. Cheaper compare-and-branch: keep the comparison result for the
      branch instead of re-reading the CR bit.
- [ ] 10. Keep guest registers in host registers instead of writing every
      change through to the `Cpu` struct.
- [ ] 11. Jump directly between regions instead of through the dispatcher.
- [x] 12. `--cpu jit-check`: every region run verified against the interpreter
      (record + replay), chapter 17.13; all JIT tests and the ROMMON boot
      run in check mode too.
- [ ] 13. The interpreter is ~2% slower with the JIT built in, even unused
      (mostly code layout; 2% unexplained, chapter 17.9).

## IDA and docs

- [x] 14. 0x43a0 is ROMMON's stack, holding a copy of its checksum
      routine it runs from RAM; ROM routines named (bootdev_checksum,
      bootdev_range, bootdev_read, copy16, cache maintenance), chapter 22.
- [x] 15. Chapter 17's worked example also shows the loop as a region
      (jump table, step counter, budget check).
