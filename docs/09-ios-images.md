# 9. Checking IOS images: PowerQUICC or "MIPS-X"?

Three images were available:

* `c1700-adventerprisek9-mz.124-8.bin` (21.3 MB)
* `c1700-k9o3sy7-mz.123-11.T2.bin` (13.3 MB)
* `c1700-k9o3sy7-mz.123-11.T2.image` (37.0 MB)

`file` reports all three as *"ELF 32-bit MSB executable, Stanford MIPS-X"*.
Question: are they really for the PowerQUICC (MPC860) CPU?

**Answer: yes, all three are PowerPC code for the MPC8xx in a Cisco 1700.**
The "MIPS-X" label is an artefact of how Cisco uses the ELF header.

Reproduce with:

```bash
python3 -I tools/ciscoimg.py c1700-k9o3sy7-mz.123-11.T2.bin [--extract ios.elf]
```

## 9.1 Why `file` says MIPS-X

The ELF header has a 16-bit field `e_machine` that names the CPU
architecture: 20 = PowerPC, 8 = MIPS, ... and **51 = Stanford MIPS-X** (a
1987 Stanford research processor nobody ships). Cisco doesn't put the CPU
there: it puts a **platform code**. For the C1700 it is **0x33 = 51**.

We know this from the boot ROM (chapter 8): `image_header_check`
(0xFFF18878) rejects an image unless `e_machine == g_platform_code`, and
`rom_init_main` sets `g_platform_code` to 0x33 when the cookie's processor
byte is 0x33. ROMMON uses the field to refuse images built for another
router family ("Wrong target platform"). `file` just looks 51 up in the
standard table.

## 9.2 The six checks

### 1. ELF header vs. the boot ROM's rules

All three pass: 32-bit big-endian, `e_type` 2, `e_machine` 0x33, header
sizes 52/32/40, one program header. The two `-mz.bin` files also follow
Cisco's layout (section headers at offset 84, file size = 84 + 40 ×
sections + section sizes, exactly).

### 2. The Cisco trailer and its checksums

In the `-mz.bin` files the last section starts with the trailer the ROM
verifies (`image_checksum_bad`, 0xFFF1DC90):

```text
feedface 0234c79c 00ca24e6 bad2f0fa 0ac0cdb4 | 504b0304 ...
magic    usize    csize    ccksum   ucksum   | "PK\3\4"
```

Both checksums (32-bit word sum with end-around carry) match. The
`.image` file has no trailer: ROMMON would accept its header, but with
TFTP_CHECKSUM=1 it would read garbage as the trailer, so it is not bootable
as is.

### 3. The payload: a plain ZIP archive

The bytes after the trailer start with `PK\3\4`: a **PKZIP** archive with
one deflate-compressed member (`C1700-K9.BIN`, `C1700-AD.BIN`). Python's
`zipfile` opens it. The decompressed size and checksum match the trailer's
"uncompressed" fields.

That is how `-mz` images work: section 1 is a small **self-decompressing
loader** (~14 KB of code at 0x80008000), the last section is the
compressed IOS. ROMMON loads both and jumps to the loader, which unzips
IOS into RAM and starts it.

**The `.image` file is byte-for-byte the decompressed member of the
12.3(11)T2 `.bin`** (identical SHA-256): someone unzipped it.

### 4. The code at the entry point

The 4-byte words at the entry point, decoded both ways:

```text
                PowerPC                     MIPS
9421fff8   stwu r1,-8(r1)              lhu   $1,-8($1)
7c0802a6   mflr r0                     <invalid>
9001000c   stw  r0,12(r1)              lbu   $1,12($zero)
3d408000   lis  r10,0x8000             <invalid>
614ac00c   ori  r10,r10,0xC00C         <invalid>
902a0000   stw  r1,0(r10)              lbu   $10,0($1)
```

As PowerPC it is a textbook function prologue (build a stack frame, save
the link register) followed by saving r1/r3/r4 (the stack and the
arguments ROMMON passes) into globals. As MIPS half the words are invalid
instructions. (LLVM can't decode real MIPS-X at all, which is another hint
that nobody targets it.)

### 5. Strings in the decompressed IOS

* "Hardware is **PowerQUICC MPC860**", "PowerQUICC(%d/%d): %s is up"
* "MPC860P CPM INTERRUPT", "MPC855T CPM INTERRUPT" (12.4(8))
* "Cisco IOS Software, **C1700** Software (C1700-K9O3SY7-M), Version 12.3(11)T2"
* The 23 "MIPS" hits in 12.4(8) are "find_dsp: MIPS required %d": voice DSP
  capacity in *millions of instructions per second*, not an architecture.

### 6. MPC8xx-only special registers

Counting `mtspr`/`mfspr` of SPRs that exist only on the MPC8xx core (the
same census done on the boot ROM in chapter 1):

```text
IMMR x35, DC_CST x21, IC_CST x15, M_TWB x5, MD_CTR x4, MD_TWC x4, MI_CTR x4,
MD_EPN x2, EID x2, MD_RPN x1, MI_EPN x1, MI_TWC x1, MI_RPN x1, M_CASID x1, NRI x1
```

IMMR, the 8xx MMU registers (MI_/MD_/M_*) and EIE/EID/NRI don't exist on
any other PowerPC family. This is MPC8xx (PowerQUICC) code specifically,
including the software-managed MMU that the emulator doesn't implement yet.

## 9.3 Practical notes

| Image | Loaded size (from 0x80000000, before heap) | RAM needed |
|-------|--------------------------------------------|------------|
| 12.3(11)T2 k9o3sy7 | 37.9 MB | 48 MB router (`--ram 48` or more) |
| 12.4(8) adventerprisek9 | 56.7 MB | 64 MB router (`--ram 64` or more) |

* The emulated flash is 4 MB (2 × Am29F016), too small to hold these
  images. `tftpdnld -r` (load into RAM) is the way to try them.
* IOS turns the 8xx MMU on (MSR[IR,DR]), but its mappings are effectively
  1:1 for what it touches at boot, so the emulator's identity-mapping
  fallback was enough to reach the setup dialog (section 9.4).

## 9.4 Dynamic test in the emulator

The strongest check is to let the real boot ROM load the real image. In the
emulator (64 MB RAM), with the ROMMON variables set as in chapter 8:

```bash
mkdir ios && ln -s $PWD/c1700-k9o3sy7-mz.123-11.T2.bin ios/ios.bin
emu/target/release/c1700emu --confreg 0x2100 --ram 64 --tftp-root ios
rommon 6 > tftpdnld -r          # with TFTP_FILE=ios.bin
```

**It boots all the way to the IOS setup dialog:**

```text
File reception completed.
program load complete, entry point: 0x80008000, size: 0xca6894
Self decompressing the image : ################################ [OK]

Smart Init is disabled. IOMEM set to: 25
Unknown Card in slot 0
Unknown Card in slot 1
PMem allocated: 57259936 bytes
...
Cisco IOS Software, C1700 Software (C1700-K9O3SY7-M), Version 12.3(11)T2, RELEASE SOFTWARE (fc1)
Compiled Thu 28-Oct-04 20:05 by cmong
Image text-base: 0x8000816C, data-base: 0x815B8B18
...
Cisco 1720 (MPC860T) processor (revision 0x00) with 55917K/9619K bytes of memory.
MPC860T processor: part number 5, mask 2
32K bytes of NVRAM.
4096K bytes of processor board System flash (Read/Write)

         --- System Configuration Dialog ---
Would you like to enter the initial configuration dialog? [yes/no]:
```

What this proves:

* ROMMON accepted the header, the platform code and both checksums.
* The loader stub — PowerPC code — ran on our MPC8xx interpreter and unzipped
  37 MB of IOS correctly (a single wrong instruction would corrupt the
  inflate output or crash).
* IOS itself ran and identified the hardware from *our models*: "part number
  5, mask 2" is the IMMR PARTNUM/MASKNUM value 0x0502 chosen in chapter 3,
  "32K bytes of NVRAM" and "4096K bytes ... flash" are our NVRAM and the
  2 × Am29F016 flash.

Timeline (wall clock): ROMMON boot ~8 s, TFTP of 13 MB ~35 s,
self-decompression ~115 s, IOS init until the setup dialog ~3.5 min;
5.75 billion emulated instructions in total.

Not right yet (next steps):

* No "FastEthernet interface" line: IOS doesn't find the onboard FEC
  interface (probably cookie fields, board id or a register we don't model).
* IOS writes to two unknown I/O bus devices at 0x68030000 and 0x68040000,
  most likely the two WIC slots ("Unknown Card in slot 0/1").
* The MMU is identity-mapped, not emulated; it worked for this boot but is
  not guaranteed for everything IOS does later.
