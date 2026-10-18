# 20. Snapshots and core dumps

IOS takes about two and a half minutes to reach `Router>`, most of it
decompressing itself. A **snapshot** saves the whole machine at any moment
and restores it in a fraction of a second, so a router that has booted
and been configured once is ready again instantly. A **core dump** writes
memory and registers as an ELF core file, for GDB and other tools.

```
Ctrl-] s                       save a snapshot (to --snapshot FILE, default c1700.snap)
Ctrl-] d                       write a core dump (to --core FILE, default c1700-core.elf)
c1700emu --restore FILE ...    start from a snapshot instead of power-on
```

Measured with IOS 12.4(8) on a 1760 with 128 MB, booted and with
FastEthernet0/0 configured:

| | |
|---|---|
| snapshot file | 70 MB (lz4-compressed RAM, flash, NVRAM and devices) |
| saving it | 0.2 s |
| restoring it | 0.13 s, at the `Router#` prompt 0.3 s after start (a boot takes ~150 s) |
| after the restore | interface still configured and up, ping to 10.0.2.2 works, `show version` uptime continues from the snapshot |

## 20.1 What is in a snapshot

Everything that decides what the machine does next:

* the CPU: all registers and SPRs, the timebase, decrementer, cycle
  counter and prescaler, the reservation, both TLBs and the MMU registers;
* the board: all RAM, the flash array and each flash chip's command
  state, the NVRAM, the MPC860's register file and DPRAM, the UPM arrays,
  the PIT, the FEC (with the PHY and its descriptor-ring cursors), pending
  CPM commands, the CPLD/IO FPGA registers and the FPGA configuration
  port, both UARTs with their receive FIFOs;
* the emulator's own bookkeeping that affects the future: the cycles not
  yet given to the devices (they are synchronised in batches), the idle
  detector's progress.

Not in it, on purpose:

* **configuration**: model, RAM size, flash size, boot ROM. A snapshot is
  restored into a machine started with the same options, and checks them:
  the ROM by a CRC-32 in the header, RAM and flash sizes while loading
  ("snapshot has a 16 MB RAM bank where this machine has 32 MB (use the
  same --ram)");
* **caches**: the board's address-decoding table, the MMU's fast
  translation cache and the JIT's translations are rebuilt;
* **the network**: whatever `--net` says now is plugged in. The built-in
  virtual host starts afresh (the router re-ARPs: the first ping after a
  restore is lost, as after plugging a cable in); a TAP device or UDP
  tunnel is reopened;
* debugging settings (breakpoints, profiles, `--watch`) and the CPU engine.

The restored flash and NVRAM count as written by the guest: they are
saved back to the `--flash` and `--nvram` files at exit.

## 20.2 How it is built: one walk for both directions

Every component with state has one method, `visit_state(&mut self, v:
&mut dyn Visitor)` (`ppc8xx/src/state.rs`), that walks its fields in a
fixed order:

```rust
pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
    let mut rx: Vec<u8> = self.rx.iter().copied().collect();
    v.bytes(&mut rx);
    self.rx = rx.into();
    for x in [&mut self.ier, &mut self.fcr, &mut self.lcr, ...] {
        v.u8(x);
    }
    v.bool(&mut self.thre_irq);
}
```

When saving, the visitor reads each field and writes it out; when
loading, it overwrites each field with what it reads. The same code does
both, so the two directions can't drift apart, which is the classic bug of
hand-written save/load pairs. Restoring *into* a machine built from the
configuration is what makes this simple: whatever isn't state (static chip
descriptions, file paths, caches) is just not visited and keeps its value.

The file (`c1700/src/snapshot.rs`) is a 16-byte header (magic
`C1700SNP`, format version, ROM CRC-32) followed by the walk, in
little-endian, through an lz4 frame (crate `lz4_flex`). Most of the 128 MB
of RAM compresses away; saving and loading run at memory speed.

**Test** (`c1700/tests/boot.rs::snapshot_restore_continues_identically`):
take a snapshot of the ROM boot right after the banner, let the original
continue to the prompt and answer a typed `confreg`, restore the snapshot
into a fresh machine and do the same. Console output, cycle count,
timebase, registers and all of RAM must be identical, and they are. A
snapshot with a different RAM size is refused.

`tools/ios_session.py` has the directives `snapshot` and `core`, and the
image argument `@` for a session that starts from `--restore` at the IOS
prompt.

## 20.3 Core dumps

`Ctrl-] d` writes an ELF32 big-endian PowerPC core file
(`c1700/src/coredump.rs`):

* a `PT_NOTE` with an `NT_PRSTATUS` note holding the registers, in the
  layout of Linux on 32-bit PowerPC (r0-r31, nip, msr, orig_r3, ctr, lr,
  xer, cr, mq, trap, dar, dsisr, result). The header says OS/ABI
  GNU/Linux, so GDB applies that layout by itself;
* one `PT_LOAD` per run of physical memory: RAM, flash, NVRAM, the IMMR
  registers and DPRAM, the boot ROM;
* with translation on, one more `PT_LOAD` per valid TLB mapping, at the
  effective address the guest uses (`p_paddr` holds the physical one), so
  IOS's own pointers can be followed in the debugger.

Nothing is stored twice: the effective-address segments point at the
bytes of the physical copy, and so do physical aliases (the board decodes
the SDRAM at more than one address). The IOS dump above is 169 MB: 128 MB
of RAM, 32 MB of flash, the rest small.

```
$ gdb -q -ex "set architecture powerpc:common" -ex "core-file c1700-core.elf"
#0  0x81bcf150 in ?? ()
(gdb) info registers pc msr lr
pc             0x81bcf150
msr            0x9032          (EE, ME, IR, DR, RI)
lr             0x806174b0
(gdb) x/2i $pc
=> 0x81bcf150:  cmplwi  r4,0
   0x81bcf154:  bgt     0x81bcf15c
(gdb) x/2xw 0x80008000          (an IOS address: TLB maps it to physical 0x8000)
0x80008000:     0x9421ffd8      0x7c0802a6
```

With the decompressed IOS ELF (`ida-ios/`) as the executable, `gdb
ida-ios/c1700-adventerprisek9-mz.124-8.elf c1700-core.elf` lines the code
up with the image.
