# 22. ROMMON runs code from its stack

While making the JIT fast (chapter 17.10), 70% of its translations were
being thrown away because something kept writing next to translated code
in low memory: physical 0x43a4, four bytes before code at 0x43a8. Watching
memory per instruction word fixed the performance problem, but the question
stayed open: what is a variable doing four bytes before code? This chapter
answers it. It is not a variable.

## 22.1 Following the trail

1. **What is the code?** The core dump of a running IOS (chapter 20) shows
   it in GDB: a 16-bit ones'-complement checksum loop (`lhz`, `add`, fold
   the carry back in). Its bytes are found in the boot ROM at 0xFFF21C44
   (and in neither the decompressed IOS image nor the compressed IOS file):
   RAM 0x43a4..0x43d8 is exactly the ROM function `checksum16_oc`, 52
   bytes. So the "variable" at 0x43a4 is the routine's **first
   instruction**.
2. **Who writes it?** `--watch 000043a0:40` saw only ROMMON's memory test.
   The writes go through the alias: the board decodes the SDRAM a second
   time at 0x80000000, and ROMMON addresses all its RAM there (its tables
   live at 0x8000xxxx). `--watch 800043a0:40` shows them:

   ```
   write 0x800043a0..dc = 0xfeedbeef           pc=0xfff03344   (stack fill pattern)
   write 0x800043a4 = 0x7d24 (2 bytes)          pc=0xfff08c90
   write 0x800043a6 = 0x2a14 (2 bytes)          pc=0xfff08c90   (7d242a14 = add r9,r4,r5)
   ...                                          pc=0xfff12554, 0xfff23c9c (register saves)
   ```

   It is **ROMMON's stack**, and the routine is copied onto it halfword by
   halfword.
3. **Why?** IDA (`tools/ida/c1700_names.py`):

   ```c
   int bootdev_checksum(int dev, int offset, int len)    // 0xFFF21C78
   {
       u16 code[66];                         // on the stack: sp+8 = 0x800043A4
       checksum_fn *sum = checksum16_oc;
       if (len > 0x10000) {
           copy16(checksum16_oc, code, 52);  // 0xFFF08C80, by halfwords
           sync_caches_for_code();           // flush D-cache, invalidate I-cache
           sum = (checksum_fn *)code;        // run it from RAM
       }
       for (...) {                           // 512 KB at a time
           bootdev_read(dev, offset, buf, n);
           s = sum(s, buf, n);
       }
       return ~s;
   }
   ```

   ROMMON checksums flash files (the 21 MB IOS image before booting it).
   The boot ROM is an 8-bit EPROM, slow to fetch instructions from; a
   checksum loop over megabytes runs much faster from RAM. So for anything
   over 64 KB, ROMMON copies the loop into a local array, does the cache
   maintenance real hardware needs before executing freshly written code
   (`dcache_flush_all` + `icache_invalidate_all`), and calls the copy.

Every call copies the routine again, which is the "write next to code"
the JIT kept seeing: ten file checks during a boot, ten invalidations.

## 22.2 What it means for the emulator

* It is classic self-modifying code, and the JIT handles it exactly: the
  watched instruction words report the copy, the region at 0x800043a8 is
  dropped and retranslated (chapter 17.7).
* The emulator has no caches, so the cache maintenance is a no-op, and
  harmless.
* Watching per instruction word (instead of per page or line) is what made
  this cheap: the stack frame below the copied code (return address,
  saved registers) is written all the time without disturbing anything.

The names (`bootdev_checksum`, `copy16`, `sync_caches_for_code`,
`dcache_flush_all`, `icache_invalidate_all`, `checksum16_oc`,
`bootdev_range`, `bootdev_read`, `bootdev_table`) are in
`tools/ida/c1700_names.py` and applied to the ROM database.
