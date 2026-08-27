#!/usr/bin/env bash
#
# End-to-end check of the phase-3 candidate window.
#
#   scripts/im-harness/candidate-test.sh
#
# frontend-test.sh proves that keys reach mozc and text comes back. This proves
# the other half: that the candidate list mozc sends is drawn, on an input
# popup surface the compositor accepted, at a plausible size, and that it goes
# away again when the conversion is committed.
#
# The frontend runs at debug level here, because the popup's evidence — the
# caret rectangle the compositor sends, the buffer sizes, the unmap — is on the
# debug lines. What the assertions read is the frontend's own log rather than
# pixels: there is no screencopy client on this machine and cosmic-comp does
# not implement wlr-screencopy, so the visual pass stays a human's job (see
# README.md, "Attended parts"). What is asserted here is everything up to the
# pixels.
#
# NEVER use `pkill -f` in here, for the reason frontend-test.sh gives.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
. "$here/lib.sh"
set +e

BIN="$repo/target/debug/cosmic-voice"
WORK="$IM_HARNESS_STATE/candidate-test"
mkdir -p "$WORK"
rm -f "$WORK"/*

frontend_pid=""
entry_pid=""
fails=0

cleanup() {
    for pid in "$entry_pid" "$frontend_pid"; do
        [ -n "$pid" ] && kill -TERM -- "-$pid" 2>/dev/null
    done
    sleep 0.5
    "$here/scratch-ibus.sh" stop >/dev/null 2>&1
    "$here/nested-comp.sh" stop >/dev/null 2>&1
}
trap cleanup EXIT

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; fails=$((fails + 1)); }

# The frontend's log with the colour escapes stripped, which is what every
# assertion below greps.
log() { sed 's/\x1b\[[0-9;]*m//g' "$WORK/frontend.log"; }

inject() { "$here/ei-inject" --bus "$HARNESS_DBUS_ADDRESS" "$@" >> "$WORK/inject.log" 2>&1; }

[ -x "$BIN" ] || { echo "build first: cargo build"; exit 2; }
[ -x "$here/ei-inject" ] || "$here/build-ei-inject.sh" || exit 2

"$here/nested-comp.sh" env >/dev/null 2>&1 || "$here/nested-comp.sh" start >/dev/null
harness_require_env_file
"$here/scratch-ibus.sh" start >/dev/null 2>&1
"$here/scratch-ibus.sh" engine mozc-on >/dev/null 2>&1
ibus_address="$(sed -n 's/^IBUS_ADDRESS=//p' "$IM_HARNESS_STATE/ibus/ibus.env")"
[ -n "$ibus_address" ] || { echo "no scratch ibus address"; exit 2; }

RUST_LOG=cosmic_voice=debug setsid "$BIN" devtest im-frontend \
    "$HARNESS_WAYLAND_DISPLAY" "$ibus_address" > "$WORK/frontend.log" 2>&1 &
frontend_pid=$!
sleep 2
grep -q 'input method bound' "$WORK/frontend.log" || {
    echo "FAIL: the frontend did not bind the input method"
    log
    exit 1
}

setsid "$here/run-entry-client.sh" candidates > "$WORK/entry.out" 2>/dev/null &
entry_pid=$!
for _ in $(seq 1 120); do grep -q '^READY' "$WORK/entry.out" 2>/dev/null && break; sleep 0.1; done
grep -q '^READY' "$WORK/entry.out" || { echo "FAIL: entry client never reported READY"; exit 1; }
sleep 2

# The font and the palette are chosen once, at bind time, and both are things
# that fail silently: a missing CJK face draws tofu, a missing theme draws the
# fallback. Both are in the log by design.
log | grep -q 'candidate font' \
    && pass "a font was chosen: $(log | sed -n 's/.*candidate font //p' | head -1)" \
    || fail "no candidate font line; the renderer never started"
log | grep -q 'candidate palette from' \
    && pass "a palette was read: $(log | sed -n 's/.*candidate palette from //p' | head -1)" \
    || fail "no candidate palette line"

# The caret rectangle only arrives if the compositor accepted the popup role
# and the application told it where its caret is.
if log | grep -q 'caret rectangle'; then
    pass "the compositor sent a caret rectangle: $(log | sed -n 's/.*caret rectangle //p' | tail -1)"
else
    fail "no text_input_rectangle; the popup surface may not have been accepted"
fi

# Romaji, then space to convert, then Tab to open the list. Space alone only
# cycles the conversion — mozc keeps the candidate window closed until asked,
# which is what its own auxiliary text (「Tabキーで選択」) is telling the user.
inject type konnnitiha sleep 800
inject key 57 sleep 800
inject key 15 sleep 1200

table="$(log | grep 'UpdateLookupTable visible=true' | tail -1)"
if [ -n "$table" ]; then
    pass "mozc sent a visible lookup table: ${table#*signal }"
else
    fail "no visible lookup table after converting"
fi

drawn="$(log | grep -c 'candidate window [0-9]')"
size="$(log | sed -n 's/.*candidate window \([0-9]*x[0-9]*\).*/\1/p' | tail -1)"
width="${size%x*}"
height="${size#*x}"
if [ "$drawn" -gt 0 ] && [ "${width:-0}" -ge 60 ] && [ "${height:-0}" -ge 30 ]; then
    pass "a $size buffer was attached and committed"
else
    fail "no plausible candidate buffer was committed (saw '$size' in $drawn draws)"
fi

# Moving the selection must produce another table, and another frame: a stale
# highlight is the most likely coalescing bug.
before="$(log | grep -c 'UpdateLookupTable visible=true')"
inject key 108 sleep 800
after="$(log | grep -c 'UpdateLookupTable visible=true')"
if [ "$after" -gt "$before" ]; then
    pass "moving the selection produced another table ($before -> $after)"
else
    fail "the down key changed nothing"
fi

# Committing must take the window down. This is the failure that would be most
# visible in daily use: a candidate list left on screen over the committed text.
inject key 28 sleep 1000
if log | grep -q 'candidate window hidden'; then
    pass "the window was hidden on commit"
else
    fail "the candidate window was never unmapped"
fi

# Which candidate was selected is mozc's business and changes with its
# dictionary, so the assertion is only that a conversion — not the romaji —
# reached the application.
committed="$(sed -n 's/^ENTRY //p' "$WORK/entry.out" | tail -1)"
case "$committed" in
    ""|*konnnitiha*) fail "the entry shows '$committed' after committing a conversion" ;;
    *) pass "the conversion committed into the entry: $committed" ;;
esac

echo "logs in $WORK"
exit $((fails > 0))
