#!/bin/bash
# TODO item 1a / docs ch. 18.2: the router on a TAP device, end to end, and
# without sudo. An unprivileged user + network namespace (`unshare -r -n`)
# makes us root inside a private network stack, where we may create tap0;
# the emulator runs in there too, restored from a snapshot of IOS at the
# prompt. The router pings the host side of tap0 and the host pings back.
#
#   tools/tests/tap_test.sh SNAPSHOT
#
# SNAPSHOT: IOS 12.4(8) on a 1760/128 MB at the prompt (Ctrl-] s, docs ch. 20).
# Exit status 0 when both pings succeed.
set -e
cd "$(dirname "$0")/../.."
snap=$(realpath "$1")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
unshare -r -n bash -e -c "
  ip link set lo up
  ip tuntap add dev tap0 mode tap
  ip addr add 192.168.100.1/24 dev tap0
  ip link set tap0 up
  LIMIT=300 python3 tools/ios_session.py $work/console.log @ -c tools/tests/tap_test.cmds -- \
    emu/target/release/c1700emu --nvram $work/nv --flash $work/fl --model 1760 --ram 128 --cpu jit \
    --restore $snap --net tap:tap0 --mac 02:17:00:00:00:01 --log $work/emu.log --log-level info
"
grep -E "Success rate|bytes from|packets transmitted|Internet  192" "$work/console.log" || true
grep -q "Success rate is [1-9]" "$work/console.log" && grep -q " 0% packet loss\| 33% packet loss" "$work/console.log" \
  && echo "TAP test: OK" || { echo "TAP test: FAILED"; tail -30 "$work/console.log"; exit 1; }
