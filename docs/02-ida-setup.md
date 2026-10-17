# 2. IDA setup

## 2.1 Loading the raw ROM

The ROM is a raw binary (no ELF header), so IDA must be told everything:

| Option | Value | Why |
|--------|-------|-----|
| Processor | `ppc` (big-endian PowerPC) | MPC860 is big-endian |
| Loading address | `0xFFF00000` | see chapter 1 |
| Entry point | `0xFFF00100` | System Reset vector |
| Segment bitness | **32-bit** | IDA's PPC module defaulted to 64-bit addressing, which mis-analyses the code (only ~200 functions were found until this was fixed) |

Via the IDA MCP (what I used):

```python
open_database(path=".../CISCO_bootrom_AT27C040.bin",
              load_options={"processor": "ppc", "file_type": "binary",
                            "image_base": 0xFFF00000, "entry_point": 0xFFF00100})
```

The resulting database is `CISCO_bootrom_AT27C040.bin.i64` next to the ROM
(`roms/`, the private companion repository).

## 2.2 Annotation scripts (reproducible)

Rather than clicking through renames, all annotations are scripts under
`tools/ida/`, so they can be re-applied to a fresh database and read as
documentation:

```python
exec(open("/path/to/cisco_rom/tools/ida/mpc860_setup.py").read())
exec(open("/path/to/cisco_rom/tools/ida/c1700_names.py").read())
```

### `mpc860_setup.py` — CPU-level knowledge

1. **IMMR segment.** The ROM writes `0xFF000000` to SPR 638 (IMMR), which
   moves every on-chip peripheral register to `0xFF000000-0xFF003FFF`. The
   script creates a segment there and names every register, using the
   `.mpc860` section of IDA's own `cfg/ppc.cfg` as the source of names and
   offsets (widths come from the manual). Absolute accesses now decompile as
   `SIMASK = 0x20000000;` instead of `MEMORY[0xFF000014] = 0x20000000;`.
2. **`IMMR_t` structure + structure offsets.** Most code does not use
   absolute addresses; it reads IMMR at run time:
   ```asm
   mfspr  r10, IMMR          ; r10 = 0xFF00xxxx (base + part number)
   clrrwi r10, r10, 15       ; keep the base
   lwz    r10, 0x288(r10)    ; ??? 
   ```
   The script tracks, inside each function, which registers hold the IMMR
   base (`mfspr rX,638` → `clrrwi`/`rlwinm`, or `lis rX,0xFF00`) and applies
   `IMMR_t` as a structure offset to the displacement, so the last line
   reads `lwz r10, IMMR_t.RSR(r10)`. (A deliberately simple linear tracker:
   it resets at branch targets, so it is conservative rather than clever.)
3. **Vector table.** Each 256-byte vector slot is converted to code and named
   `vec_0700_Program` etc.; the handler address loaded by the trampoline's
   `lis/ori` pair gets `exc_<Name>`. A handler shared by many vectors is
   named `exc_Unexpected`.
4. **SPR comments.** IDA's generic PPC profile prints `mtspr 0x27E, r10`.
   Every `mtspr`/`mfspr` gets a comment with the 8xx name and meaning, e.g.
   `IMMR: internal memory map register: [0:15]=ISB ...`.

### `c1700_names.py` — board-level conclusions

* Segments + register names for the board devices on chip select 7:
  `NVRAM` (0x68000000), `CPLD` (0x68020000), `UART` (0x68050000, the 16550
  registers `UART_LSR`, `UART_LCR`...), and `RAM_HI` (0x80000000, where
  ROMMON keeps its data — see chapter 3).
* ~60 functions renamed **with prototypes and a comment explaining the
  evidence** (e.g. `uart_init`, `dram_init_and_size`, `fatal_error_blink`,
  `cookie_get`, `nvram_write`, `flash_probe_banks`, `amd_flash_cmd`).
* Globals typed (`upm_table_sdram_init[64]`, `console_baud_table[8]`,
  `g_cookie[132]`) and data tables (`cookie_field_table`,
  `flash_device_table`).
* Second-stage exception handlers reached through `rfi` (pattern
  `lis/ori rX; mtsrr0 rX; ...; rfi`) are found and turned into functions.

### Headless IDA (`tools/ida/decompile.py`)

When the IDA MCP server was unavailable, I used **idalib** (IDA as a Python
library, from IDA's own virtualenv) directly:

```bash
cp roms/CISCO_bootrom_AT27C040.bin.i64 /tmp/copy.i64          # don't touch the real one
~/.idapro/venv/bin/python tools/ida/decompile.py /tmp/copy.i64 0xFFF1EF1C   # pseudocode
~/.idapro/venv/bin/python tools/ida/decompile.py /tmp/copy.i64 --disasm 0xFFF1E4A0
# apply the scripts to the real database (only while nothing else has it open):
~/.idapro/venv/bin/python tools/ida/decompile.py roms/CISCO_bootrom_AT27C040.bin.i64 \
    --script tools/ida/mpc860_setup.py --script tools/ida/c1700_names.py --save
```

If Hex-Rays reports "call analysis failed", `--disasm` still prints the
annotated assembly. If a requested address sits inside a bigger function,
the helper splits it so the address becomes a function start.

## 2.3 Techniques worth remembering

* **Decompile first, then read assembly where the decompiler lies.** Hex-Rays
  assumes memory doesn't change behind its back, so in the DRAM sizing code it
  "simplified" `size = *(u32*)0` to a constant — exactly the read whose value
  changes through address aliasing (chapter 3).
* **Negative constants are addresses.** The decompiler shows
  `sub_FFF246D0(-894052)`; `-894052 & 0xFFFFFFFF = 0xFFF25B9C`, a string.
  (I mis-converted one of these once — `-2147457820` is `0x800064E4`, not
  `0x800063E4` — double-check with Python: `hex(-2147457820 & 0xffffffff)`.
  The stale label later made the decompiler print `g_cookie[128]` for what is
  really `g_platform_code` at 0x80006464; always confirm against the raw
  instruction, here `lwz r9, 0x6464(r9)`.)
* **`lis`+`addi` pairs don't create xrefs** for pointers built at run time.
  A tiny scanner that looks for `lis rX,ha` followed by an instruction using
  `lo(rX)` finds who references a string (that's how the flash driver and the
  cookie editor were located).
* **Strings printed on error paths name the function.** "Bad checksum on
  cookie structure, resorting to backup copy" told us the NVRAM block at
  offset 0 is the cookie.
* **Use the emulator as an analysis tool.** `--profile` (chapter 7) showed
  that 73 % of the boot is `bzero_bytes`, and the GDB stub found the flash
  probing stack overflow (chapter 4) in two commands.

## 2.4 If the IDA MCP gets blocked

In Claude Code's auto mode, IDB-modifying calls were initially refused by the
safety classifier. The fix was an explicit allow rule with the tool's
internal name in `~/.claude/settings.json`:

```json
"permissions": { "allow": ["mcp__plugin_ida-mcp_ida__execute_python"] }
```

(`"IDA"` does not match anything; `mcp__plugin_ida-mcp_ida` allows the whole
server.)
