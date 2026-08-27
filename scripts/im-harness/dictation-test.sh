#!/usr/bin/env bash
#
# End-to-end check of the phase-5 turn-taking between typing and dictation.
#
#   scripts/im-harness/dictation-test.sh
#
# Same scaffolding as frontend-test.sh — nested cosmic-comp, isolated
# ibus-daemon with mozc, a GTK entry, keys over libei — plus a fifo standing in
# for the microphone. `devtest im-frontend --control-fifo` reads
# newline-delimited JSON commands off it and feeds them to the frontend
# exactly as the dictation engine would (a bare DictationCmd is accepted as
# well as the tagged ImCmd form property-test.sh uses), so the whole
# turn-taking path runs with no audio, no recogniser and no live session. See
# README.md, "Safety invariants".
#
# What it proves, in order:
#
#   1. the keyboard's turn  mozc has an uncommitted こんにちは in preedit
#   2. Begin finishes it    the half-typed conversion is committed, not thrown
#                           away, and the context is reset behind it
#   3. partials preedit     provisional text appears as preedit and revises
#   4. commit lands         the final transcript is committed as text
#   5. typing resumes       mozc converts again afterwards, so nothing about
#                           the dictation turn left the key path broken
#
# NEVER use `pkill -f` in here: this script's own command line contains every
# pattern you would want to match, and the user's cosmic-voice applet is called
# `cosmic-voice` too. Everything is killed by recorded pid.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
. "$here/lib.sh"
set +e

BIN="$repo/target/debug/cosmic-voice"
WORK="$IM_HARNESS_STATE/dictation-test"
FIFO="$WORK/dictation.fifo"
mkdir -p "$WORK"
rm -f "$WORK"/*

ENTER=28

frontend_pid=""
entry_pid=""
fails=0

cleanup() {
    for pid in "$entry_pid" "$frontend_pid"; do
        [ -n "$pid" ] && kill -TERM -- "-$pid" 2>/dev/null
    done
    sleep 0.5
    rm -f "$FIFO"
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

# The last committed contents of the entry, and the last preedit it was shown.
entry_text()   { sed -n 's/^ENTRY //p'   "$WORK/entry.out" | tail -1; }
preedit_text() { sed -n 's/^PREEDIT //p' "$WORK/entry.out" | tail -1; }

inject() { "$here/ei-inject" --bus "$HARNESS_DBUS_ADDRESS" "$@" >> "$WORK/inject.log" 2>&1; }

# One dictation command. Each write is its own open/write/close on the fifo,
# which is what the reader's reopen loop is built for.
dictate() { printf '%s\n' "$1" > "$FIFO"; sleep 0.4; }

[ -x "$BIN" ] || { echo "build first: cargo build"; exit 2; }
[ -x "$here/ei-inject" ] || "$here/build-ei-inject.sh" || exit 2

"$here/nested-comp.sh" env >/dev/null 2>&1 || "$here/nested-comp.sh" start >/dev/null
harness_require_env_file
"$here/scratch-ibus.sh" start >/dev/null 2>&1
"$here/scratch-ibus.sh" engine mozc-on >/dev/null 2>&1
ibus_address="$(sed -n 's/^IBUS_ADDRESS=//p' "$IM_HARNESS_STATE/ibus/ibus.env")"
[ -n "$ibus_address" ] || { echo "no scratch ibus address"; exit 2; }

# Created before the frontend starts, because the reader refuses a path that is
# not there rather than creating one and racing whoever writes to it.
mkfifo "$FIFO" || { echo "could not create $FIFO"; exit 2; }

setsid "$BIN" devtest im-frontend "$HARNESS_WAYLAND_DISPLAY" "$ibus_address" \
    --control-fifo "$FIFO" \
    > "$WORK/frontend.log" 2>&1 &
frontend_pid=$!
sleep 2
grep -q 'input method bound' "$WORK/frontend.log" || {
    echo "FAIL: the frontend did not bind the input method"
    sed 's/\x1b\[[0-9;]*m//g' "$WORK/frontend.log"
    exit 1
}

setsid "$here/run-entry-client.sh" dictation > "$WORK/entry.out" 2>/dev/null &
entry_pid=$!
for _ in $(seq 1 120); do grep -q '^READY' "$WORK/entry.out" 2>/dev/null && break; sleep 0.1; done
grep -q '^READY' "$WORK/entry.out" || { echo "FAIL: entry client never reported READY"; exit 1; }
sleep 2
grep -q 'activate ' "$WORK/frontend.log" || {
    echo "FAIL: the frontend was never activated; is another input method bound?"
    exit 1
}

# 1. The keyboard's turn: romaji into mozc, deliberately *not* committed. This
#    is the state the design says a Begin must not throw away.
inject type konnnitiha sleep 1000
check "mozc has an uncommitted preedit" "こんにちは" "$(preedit_text)"
check "and nothing is committed yet" "" "$(entry_text)"

# 2. Begin. The half-typed conversion is finished into the field, and only then
#    is the context reset — the order is what makes this different from
#    dropping the preedit, which is what IBus's own bridge does.
dictate '"Begin"'
check "Begin committed the pending conversion" "こんにちは" "$(entry_text)"

# 3. Partials are preedit, revised as often as the recogniser changes its mind.
dictate '{"Partial":"hello wor"}'
check "the first partial is shown as preedit" "hello wor" "$(preedit_text)"
dictate '{"Partial":"hello world"}'
check "the partial revised in place" "hello world" "$(preedit_text)"

# 4. The final transcript, replacing the preedit in one update.
dictate '{"Commit":"hello world "}'
check "the transcript was committed" "こんにちはhello world " "$(entry_text)"

# 5. The keyboard has the turn back. Nothing about entering and leaving
#    dictation may leave the key path — or mozc's context — in a state where
#    the next conversion fails.
inject type aiueo sleep 1000
check "typing resumed through mozc" "あいうえお" "$(preedit_text)"
inject key $ENTER sleep 800
check "and commits into the same field" "こんにちはhello world あいうえお" "$(entry_text)"

echo "logs in $WORK"
exit $((fails > 0))
