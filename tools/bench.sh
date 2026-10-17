#!/bin/bash
# CPU engine speed benchmark (docs ch. 16): power-on to 3 billion instructions
# with IOS 12.4(8) in the internal flash, i.e. ROMMON plus nearly all of
# IOS's self-decompression, no console. Prints MIPS.
#
# The ROM accelerators and idle detection are off, so every counted
# instruction is really executed: the number measures the CPU engine.
#
#   tools/bench.sh [extra c1700emu options, e.g. --cpu jit]
#   BUDGET=1000000000 tools/bench.sh
#   PERF="perf record -F 999 -o bench.perf.data --" tools/bench.sh
#
# Needs a release build and the private roms/ checkout.
set -e
cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
image=roms/c1700-adventerprisek9-mz.124-8.bin
budget=${BUDGET:-3000000000}
s=$(date +%s.%N)
${PERF:-} emu/target/release/c1700emu --nvram "$work/nvram.bin" --flash "$work/flash.bin" \
    --flash-file "$image" --model 1760 --ram 128 --confreg 0x2102 --no-stdio -n "$budget" \
    --no-fast-forward --idle off --log "$work/emu.log" --log-level warn "$@" \
    < /dev/null > /dev/null 2>&1
e=$(date +%s.%N)
python3 -c "t = $e - $s; print(f'{$budget/1e6:.0f}M instructions in {t:.1f} s = {$budget/1e6/t:.1f} MIPS')"
