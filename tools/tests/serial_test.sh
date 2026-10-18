#!/bin/bash
# TODO item 2a / docs ch. 23: two emulated 1760s, each with a WIC-1T, their
# serial ports cabled back to back through a UDP tunnel. Both configure
# Serial0/0 (HDLC, the default encapsulation), the line protocol comes up
# on the keepalives, and each router pings the other.
#
#   tools/tests/serial_test.sh [extra c1700emu options, e.g. --cpu interp]
#   KEEP=DIR tools/tests/serial_test.sh    # keep the consoles and logs in DIR
#
# Exit status 0 when the line protocol is up on both and the pings succeed.
set -e
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
image=roms/c1700-adventerprisek9-mz.124-8.bin
port=$((31000 + $$ % 1000))

router() { # NAME ADDRESS PEER LPORT RPORT MAC SERIAL KEEPALIVE
    cat > "$work/$1.cmds" <<CMDS
enable
terminal length 0
configure terminal
interface Serial0/0
ip address $2 255.255.255.0
keepalive $8
no shutdown
end
sleep 40
show interfaces Serial0/0
ping $3
wait Success rate
show controllers serial 0/0
sleep 20
CMDS
    LIMIT=600 python3 tools/ios_session.py "$work/$1.log" - -c "$work/$1.cmds" -- \
        emu/target/release/c1700emu --nvram "$work/$1.nv" --flash "$work/$1.fl" --flash-file "$image" \
        --model 1760 --ram 128 --confreg 0x2102 --wic0 wic-1t --serial0 "udp:$4:127.0.0.1:$5" \
        --mac "$6" --serial "$7" --log "$work/$1.emu.log" --log-level info "${@:9}" > /dev/null 2>&1
}

# The emulator is deterministic: two identically configured routers run
# cycle for cycle alike, and their keepalives carry the same "magic"
# value (a counter plus the uptime), which IOS takes for its own echo: a
# looped line (docs ch. 23). Real routers never run in lockstep; here B
# sends its keepalives every 7 s instead of 10 (and each router has its
# own MAC address and serial number).
router A 10.9.9.1 10.9.9.2 $port $((port + 1)) 02:17:00:00:00:0a EMU1234A001 10 "$@" &
a=$!
router B 10.9.9.2 10.9.9.1 $((port + 1)) $port 02:17:00:00:00:0b EMU1234B002 7 "$@" &
b=$!
wait $a $b || true

ok=0
for r in A B; do
    echo "== router $r"
    grep -E "Serial0/0 is|Success rate|DCD=|cable attached|V\.35|clocks" "$work/$r.log" | head -8
    grep -q "Serial0/0 is up, line protocol is up *$" "$work/$r.log" && grep -q "Success rate is [1-9]" "$work/$r.log" || ok=1
done
[ -n "$KEEP" ] && cp -r "$work" "$KEEP" && echo "logs kept in $KEEP"
[ $ok = 0 ] && echo "serial test: OK" || echo "serial test: FAILED"
exit $ok
