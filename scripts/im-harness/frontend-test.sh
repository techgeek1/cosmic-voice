#!/usr/bin/env bash
#
# End-to-end check of the phase-2 Wayland input-method frontend.
#
#   scripts/im-harness/frontend-test.sh
#
# Starts the nested compositor, an isolated ibus-daemon with mozc, the frontend
# bound to the nested display, and a GTK entry, then drives the whole thing
# with libei keystrokes and asserts on what comes out of the entry. Nothing
# here touches the live session: see README.md, "Safety invariants".
#
# What it proves, in order:
#
#   1. mozc conversion   romaji in, こんにちは committed into the entry
#   2. commit-as-text    with the xkb engine, a plain key arrives as a commit
#   3. passthrough       Ctrl-A reaches the application as a key, not as text
#   4. key repeat        a held key repeats at the compositor's rate, not the
#                        forty-times-too-fast rate IBus's own bridge uses
#   5. daemon loss       killing ibus-daemon leaves the frontend running and
#                        keys reaching the application raw
#
# NEVER use `pkill -f` in here: this script's own command line contains every
# pattern you would want to match, and the user's cosmic-voice applet is called
# `cosmic-voice` too. Everything is killed by recorded pid.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
. "$here/lib.sh"
set +e

BIN="$repo/target/debug/cosmic-voice"
WORK="$IM_HARNESS_STATE/frontend-test"
mkdir -p "$WORK"
rm -f "$WORK"/*

frontend_pid=""
entry_pid=""
second_pid=""
fails=0

cleanup() {
    for pid in "$entry_pid" "$second_pid" "$frontend_pid"; do
        [ -n "$pid" ] && kill -TERM -- "-$pid" 2>/dev/null
    done
    sleep 0.5
    "$here/scratch-ibus.sh" stop >/dev/null 2>&1
    "$here/nested-comp.sh" stop >/dev/null 2>&1
}
trap cleanup EXIT

check() {
    local what="$1" expected="$2" got="$3"
    if [ "$got" = "$expected" ]; then
        echo "PASS: $what"
    else
        echo "FAIL: $what"
        echo "      expected: $expected"
        echo "      got:      $got"
        fails=$((fails + 1))
    fi
}

# The last committed contents of the entry, or the empty string.
entry_text() { sed -n 's/^ENTRY //p' "$WORK/entry.out" | tail -1; }

inject() { "$here/ei-inject" --bus "$HARNESS_DBUS_ADDRESS" "$@" >> "$WORK/inject.log" 2>&1; }

[ -x "$BIN" ] || { echo "build first: cargo build"; exit 2; }
[ -x "$here/ei-inject" ] || "$here/build-ei-inject.sh" || exit 2

"$here/nested-comp.sh" env >/dev/null 2>&1 || "$here/nested-comp.sh" start >/dev/null
harness_require_env_file
"$here/scratch-ibus.sh" start >/dev/null 2>&1
"$here/scratch-ibus.sh" engine mozc-on >/dev/null 2>&1
ibus_address="$(sed -n 's/^IBUS_ADDRESS=//p' "$IM_HARNESS_STATE/ibus/ibus.env")"
[ -n "$ibus_address" ] || { echo "no scratch ibus address"; exit 2; }

# The frontend. It refuses the live display itself, and the display here is an
# absolute path that lib.sh has already checked is not the live socket.
setsid "$BIN" devtest im-frontend "$HARNESS_WAYLAND_DISPLAY" "$ibus_address" \
    > "$WORK/frontend.log" 2>&1 &
frontend_pid=$!
sleep 2
grep -q 'input method bound' "$WORK/frontend.log" || {
    echo "FAIL: the frontend did not bind the input method"
    sed 's/\x1b\[[0-9;]*m//g' "$WORK/frontend.log"
    exit 1
}

setsid "$here/run-entry-client.sh" test > "$WORK/entry.out" 2>/dev/null &
entry_pid=$!
for _ in $(seq 1 120); do grep -q '^READY' "$WORK/entry.out" 2>/dev/null && break; sleep 0.1; done
grep -q '^READY' "$WORK/entry.out" || { echo "FAIL: entry client never reported READY"; exit 1; }
sleep 2
grep -q 'activate ' "$WORK/frontend.log" || {
    echo "FAIL: the frontend was never activated; is another input method bound?"
    exit 1
}

# 1. mozc: romaji through the engine, committed on Return.
inject type konnnitiha sleep 800 key 28 sleep 800
check "mozc committed こんにちは" "こんにちは" "$(entry_text)"

# 2. the xkb engine: unhandled plain keys become committed text (routing rule 3).
"$here/scratch-ibus.sh" engine "xkb:us::eng" >/dev/null 2>&1
sleep 1
inject type " hello" sleep 600
check "xkb engine committed plain text" "こんにちは hello" "$(entry_text)"

# 3. a modified key is the application's. Ctrl-A selects all in a GTK entry, so
#    the next character replaces everything — which only happens if the key
#    reached the entry as a key rather than as text.
inject down 29 key 30 up 29 sleep 400 type X sleep 600
check "Ctrl-A reached the application" "X" "$(entry_text)"

# 4. repeat. 1.5s of a held key at the usual 600ms delay and 25/s rate is one
#    press plus twenty-two repeats; at the bridge's rate-as-period bug it would
#    be about thirty-six.
inject down 48 sleep 1500 up 48 sleep 600
repeats="$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/frontend.log" | grep -c 'key 48 press')"
typed="$(entry_text)"
if [ "$repeats" -ge 15 ] && [ "$repeats" -le 30 ]; then
    echo "PASS: key repeat fired $repeats times in 1.5s"
else
    echo "FAIL: key repeat fired $repeats times in 1.5s (expected 15-30)"
    fails=$((fails + 1))
fi
case "$typed" in
    Xbbbbbbbbbb*) echo "PASS: the repeats reached the entry" ;;
    *) echo "FAIL: the entry shows '$typed' after a held key"; fails=$((fails + 1)) ;;
esac

# 5. losing the daemon. Keys must keep reaching the application, as keys: the
#    entry client prints KEY lines for real key events and only ENTRY lines for
#    committed text, so the KEY line is the assertion.
kill -TERM -- "-$(cat "$IM_HARNESS_STATE/ibus/ibus.pgid")" 2>/dev/null
sleep 2
if kill -0 "$frontend_pid" 2>/dev/null; then
    echo "PASS: the frontend survived ibus-daemon dying"
else
    echo "FAIL: the frontend exited when ibus-daemon died"
    fails=$((fails + 1))
fi
inject type q sleep 600
if grep -q '^KEY q' "$WORK/entry.out"; then
    echo "PASS: keys pass through raw with no daemon"
else
    echo "FAIL: no raw key reached the application after the daemon died"
    fails=$((fails + 1))
fi

echo "logs in $WORK"
exit $((fails > 0))
