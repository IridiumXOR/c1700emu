# TODO

Open work on the C1700 emulator, roughly by priority inside each group.
Chapter references are to `docs/`.

## Emulator: hardware and features

- [ ] **1. Network bridge to the real LAN** *(in progress)*: only the
      built-in virtual network exists (one host 10.0.2.2: ARP, ping, TFTP).
- [ ] **2. WIC/VIC slot hardware** *(in progress)*: IOS writes to unknown
      devices at 0x68030000, 0x68040000, 0x68080000, 0x680B0000; the slot ID
      EEPROMs answer "empty"; the SPI chip selects for slots 2/3 aren't modelled.
- [ ] **3. IO FPGA registers** 0x0A, 0x0C, 0x1C, 0x5C, 0x6C *(in progress)*:
      accessed by IOS, meaning unknown.
- [ ] **4. The 5 machine checks IOS takes during boot** *(in progress)*:
      never investigated.
- [ ] 5. `--aux-tcp` (AUX port over telnet) never tested with IOS.
- [ ] 6. **Snapshots**: save the machine at `Router>`, restore in seconds
      (first item of chapter 14's plan, never done; biggest usability win).
- [ ] 7. Warp idle mode made IOS's console go silent and was removed; cause
      unknown (chapter 15.5).

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
- [ ] 15. Chapter 17's worked example shows code from the first
      (one block per function) JIT version.
