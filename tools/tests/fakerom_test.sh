#!/bin/bash
# Docs ch. 24: boot IOS 12.4(8) with the built-in boot ROM (--rom builtin)
# on a fresh NVRAM, run `show version`, configure the Ethernet port, ping
# the virtual host, `write memory` and `reload`; the reload must come back
# through the built-in ROM with the saved hostname, and so must a second
# power-on of a new emulator process on the same NVRAM file. Skips (exit 0)
# when the IOS image is missing.
#
#   tools/tests/fakerom_test.sh [extra c1700emu options, e.g. --cpu interp]
#
# Environment: IOS_IMAGE (default roms/c1700-adventerprisek9-mz.124-8.bin),
# C1700EMU (default emu/target/release/c1700emu); SCALE as for
# tools/ios_session.py (e.g. SCALE=10 LIMIT=3000 with --cpu interp).
set -e
cd "$(dirname "$0")/../.."
image=${IOS_IMAGE:-roms/c1700-adventerprisek9-mz.124-8.bin}
emu=${C1700EMU:-emu/target/release/c1700emu}
if [ ! -f "$image" ]; then
    echo "fakerom test: SKIPPED ($image not found)"
    exit 0
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
opts=(--rom builtin --nvram "$work/nv.bin" --flash "$work/fl.bin" --flash-file "$image"
      --model 1760 --ram 128 --confreg 0x2102 --cpu jit --idle off "$@")
cat > "$work/cmds1" <<'EOF'
enable
terminal length 0
show version
configure terminal
hostname fakerom
interface FastEthernet0/0
ip address 10.0.2.15 255.255.255.0
no shutdown
end
write memory
sleep 5
ping 10.0.2.2
wait Success rate
send reload
wait confirm
send
wait Press RETURN
send
wait fakerom>
enable
terminal length 0
show version
EOF
printf 'enable\nterminal length 0\nshow version\n' > "$work/cmds2"
LIMIT=${LIMIT:-600} python3 tools/ios_session.py "$work/boot1.log" - -c "$work/cmds1" -- \
    "$emu" "${opts[@]}" --log "$work/emu1.log" > /dev/null 2>&1 || true
IOS_HOSTNAME=fakerom LIMIT=${LIMIT:-600} python3 tools/ios_session.py "$work/boot2.log" - -c "$work/cmds2" -- \
    "$emu" "${opts[@]}" --log "$work/emu2.log" > /dev/null 2>&1 || true
grep -E "setup dialog|prompt:|Success rate|returned to ROM" "$work/boot1.log" "$work/boot2.log" | sed "s|$work/||"
ok=1
grep -q "^ROM: c1700emu built-in boot ROM" "$work/boot1.log" || ok=0
grep -q "Success rate is [1-9]" "$work/boot1.log" || ok=0
grep -q "System returned to ROM by reload" "$work/boot1.log" || ok=0
grep -q "prompt: 'fakerom>'" "$work/boot2.log" || ok=0
grep -q "System returned to ROM by power-on" "$work/boot2.log" || ok=0
if grep -q "not implemented\|system halted" "$work/emu1.log" "$work/emu2.log" "$work/boot1.log" "$work/boot2.log"; then
    grep -h "not implemented\|system halted" "$work"/emu?.log "$work"/boot?.log | sort -u
fi
[ $ok = 1 ] && echo "fakerom test: OK" && exit 0
echo "fakerom test: FAILED"; tail -20 "$work/boot1.log" "$work/boot2.log"; exit 1
