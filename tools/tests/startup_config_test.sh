#!/bin/bash
# Docs ch. 7.7: boot IOS on a fresh NVRAM with --startup-config
# tools/examples/router.cfg (given through a --config file): IOS must skip
# the setup dialog, take the hostname and the interface address from it, and
# ping the virtual host; --nvram-info must show the configuration.
#
#   tools/tests/startup_config_test.sh [extra c1700emu options]
set -e
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/emu.toml" <<TOML
model = "1760"
ram = 128
nvram = "$work/nv.bin"
flash = "$work/fl.bin"
flash-file = ["roms/c1700-adventerprisek9-mz.124-8.bin"]
confreg = "0x2102"
startup-config = "tools/examples/router.cfg"
cpu = "jit"
idle = "off"
TOML
printf 'ping 10.0.2.2\nwait Success rate\n' > "$work/cmds"
IOS_HOSTNAME=emu1 LIMIT=300 python3 tools/ios_session.py "$work/console.log" - -c "$work/cmds" -- \
    emu/target/release/c1700emu --config "$work/emu.toml" --log "$work/emu.log" "$@" > /dev/null 2>&1 || true
emu/target/release/c1700emu --nvram "$work/nv.bin" --nvram-info > "$work/info.txt"
grep -E "setup dialog|prompt|Success rate" "$work/console.log" | head -4
grep -E "configuration register|hostname" "$work/info.txt"
grep -q "prompt: 'emu1>'" "$work/console.log" && grep -q "Success rate is [1-9]" "$work/console.log" \
    && grep -q "hostname emu1" "$work/info.txt" && echo "startup-config test: OK" && exit 0
echo "startup-config test: FAILED"; tail -20 "$work/console.log"; exit 1
