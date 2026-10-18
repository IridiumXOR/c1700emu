#!/bin/bash
# Docs ch. 23: two emulated 1760s with a WIC-2T each, cabled twice:
#   A Serial0/0 (DCE, clock rate 64000) <-> B Serial0/0 (DTE)   10.1.1.0/24
#   A Serial0/1 (DTE)                   <-> B Serial0/1 (DCE)   10.2.2.0/24
# Both lines must come up and each router must ping the other on both.
#
#   tools/tests/serial_2t_test.sh [extra c1700emu options, e.g. --cpu interp]
#   KEEP=DIR tools/tests/serial_2t_test.sh    # keep the consoles and logs in DIR
set -e
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
image=roms/c1700-adventerprisek9-mz.124-8.bin
p=$((33000 + $$ % 1000 * 4))

router() { # NAME HOST(1|2) KEEPALIVE MAC SERIAL0 SERIAL0-1 DCE_IF [options]
    cat > "$work/$1.cmds" <<CMDS
enable
terminal length 0
configure terminal
interface Serial0/0
ip address 10.1.1.$2 255.255.255.0
keepalive $3
no shutdown
interface Serial0/1
ip address 10.2.2.$2 255.255.255.0
keepalive $3
no shutdown
interface $7
clock rate 64000
end
sleep 40
show ip interface brief
ping 10.1.1.$((3 - $2))
wait Success rate
ping 10.2.2.$((3 - $2))
wait Success rate
show controllers serial 0/0
show controllers serial 0/1
sleep 20
CMDS
    LIMIT=600 python3 tools/ios_session.py "$work/$1.log" - -c "$work/$1.cmds" -- \
        emu/target/release/c1700emu --nvram "$work/$1.nv" --flash "$work/$1.fl" --flash-file "$image" \
        --model 1760 --ram 128 --confreg 0x2102 --wic0 wic-2t --serial0 "$5" --serial0-1 "$6" \
        --mac "$4" --log "$work/$1.emu.log" --log-level info "${@:8}" > /dev/null 2>&1
}

# (Different keepalive periods: two lockstep emulators would otherwise send
# identical keepalives, which IOS reads as a looped line; see serial_test.sh.)
router A 1 10 02:17:00:00:00:0a "udp:$p:127.0.0.1:$((p + 1)),dce" "udp:$((p + 2)):127.0.0.1:$((p + 3))" Serial0/0 "$@" &
a=$!
router B 2 7 02:17:00:00:00:0b "udp:$((p + 1)):127.0.0.1:$p" "udp:$((p + 3)):127.0.0.1:$((p + 2)),dce" Serial0/1 "$@" &
b=$!
wait $a $b || true

ok=0
for r in A B; do
    echo "== router $r"
    grep -E "^Serial0/[01] .*up|Success rate|DCE V.35|DTE V.35" "$work/$r.log" | head -8
    [ "$(grep -c "^Serial0/[01] .* up *up" "$work/$r.log")" = 2 ] || ok=1
    [ "$(grep -c "Success rate is [1-9]" "$work/$r.log")" = 2 ] || ok=1
done
[ -n "$KEEP" ] && cp -r "$work" "$KEEP" && echo "logs kept in $KEEP"
[ $ok = 0 ] && echo "serial 2T test: OK" || echo "serial 2T test: FAILED"
exit $ok
