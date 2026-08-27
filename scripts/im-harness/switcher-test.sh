#!/usr/bin/env bash
#
# End-to-end check of the phase-4 panel duties: the engine-switch trigger and
# the engine cycle.
#
#   scripts/im-harness/switcher-test.sh
#
# Starts the nested compositor, an isolated ibus-daemon with mozc, the frontend
# bound to the nested display with a config that names a known trigger and a
# known two-engine cycle, and a GTK entry. Then it injects the trigger over
# libei and asserts on what the daemon says the global engine is. Nothing here
# touches the live session: see README.md, "Safety invariants".
#
# The config override is the point of the exercise. The scratch daemon runs
# with --config disable and an XDG_CONFIG_HOME of its own, so its dconf is
# empty and `preload-engines` is empty with it; and the frontend reads dconf
# from whatever environment it inherits, which on this machine is the user's
# real settings. Neither is a fixture. `ibus_triggers` and `ibus_engines` in
# the config file make both known values, which is what lets this assert on
# exact engine names.
#
# What it proves, in order:
#
#   1. registration    the (ya(uuu)) property Set is accepted by the daemon
#   1b. startup engine  the scratch daemon starts with NO global engine, as a
#                      live `ibus-daemon --panel disable` does, and the frontend
#                      chooses the head of the cycle the way ibus-ui-gtk3 did
#   2. forward cycle   Ctrl+Alt+Space moves xkb:us::eng -> mozc-on
#   3. it cycles       and again, back to xkb:us::eng
#   4. backward flag   Shift+Ctrl+Alt+Space reports is_backward, which is the
#                      keycode-slot encoding making the round trip
#   5. the switch is real  mozc converts konnnitiha after the switch, which it
#                      could not do if the engine had not actually changed
#
# Forward and backward land on the same engine with a two-entry cycle, so the
# direction itself is a unit test (`im::switcher::tests::cycles_both_ways`);
# what only a real daemon can prove is that the backward registration exists
# and that the flag survives the wire, which is assertion 4.
#
# NEVER use `pkill -f` in here: this script's own command line contains every
# pattern you would want to match, and the user's cosmic-voice applet is called
# `cosmic-voice` too. Everything is killed by recorded pid.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
. "$here/lib.sh"
set +e

BIN="$repo/target/debug/cosmic-voice"
WORK="$IM_HARNESS_STATE/switcher-test"
mkdir -p "$WORK"
rm -f "$WORK"/*

# evdev codes, which is what ei-inject speaks.
CTRL=29
ALT=56
SHIFT=42
SPACE=57
ENTER=28

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

# The frontend's log with the tracing colour escapes taken out.
frontend_log() { sed 's/\x1b\[[0-9;]*m//g' "$WORK/frontend.log"; }

# The last committed contents of the entry, or the empty string.
entry_text() { sed -n 's/^ENTRY //p' "$WORK/entry.out" | tail -1; }

inject() { "$here/ei-inject" --bus "$HARNESS_DBUS_ADDRESS" "$@" >> "$WORK/inject.log" 2>&1; }

# The scratch daemon's global engine. `ibus engine` with no argument reads it.
global_engine() { "$here/scratch-ibus.sh" run ibus engine 2>/dev/null | tr -d '\r\n'; }

# Waits up to three seconds for the daemon to report an engine. The switch is
# three D-Bus hops away from the keystroke — key to us, SetGlobalEngine to the
# daemon, GlobalEngineChanged back — and the daemon dispatches its broadcasts
# from a GLib idle callback, so this is a poll rather than a sleep.
await_engine() {
    local want="$1" got=""
    for _ in $(seq 1 30); do
        got="$(global_engine)"
        [ "$got" = "$want" ] && break
        sleep 0.1
    done
    echo "$got"
}

[ -x "$BIN" ] || { echo "build first: cargo build"; exit 2; }
[ -x "$here/ei-inject" ] || "$here/build-ei-inject.sh" || exit 2

"$here/nested-comp.sh" env >/dev/null 2>&1 || "$here/nested-comp.sh" start >/dev/null
harness_require_env_file
"$here/scratch-ibus.sh" start >/dev/null 2>&1
# Deliberately no `scratch-ibus.sh engine` here: a fresh daemon with no panel
# has no global engine (phase-5 finding 11), and choosing one is now the
# frontend's job. Assertion 1b below is that it did.
ibus_address="$(sed -n 's/^IBUS_ADDRESS=//p' "$IM_HARNESS_STATE/ibus/ibus.env")"
[ -n "$ibus_address" ] || { echo "no scratch ibus address"; exit 2; }

# Ctrl+Alt+Space rather than the schema default <Super>space: cosmic-comp
# filters its own compositor shortcuts before the input-method grab ever sees
# them, and Super combinations are where those live.
cat > "$WORK/config.ron" <<RON
(
    ibus_triggers: ["<Control><Alt>space"],
    ibus_engines:  ["xkb:us::eng", "mozc-on"],
)
RON

setsid "$BIN" devtest im-frontend "$HARNESS_WAYLAND_DISPLAY" "$ibus_address" \
    --config "$WORK/config.ron" \
    > "$WORK/frontend.log" 2>&1 &
frontend_pid=$!
sleep 2
grep -q 'input method bound' "$WORK/frontend.log" || {
    echo "FAIL: the frontend did not bind the input method"
    frontend_log
    exit 1
}

# 1. The daemon accepted the (ya(uuu)) property Set. A rejected signature or a
#    malformed value surfaces here and nowhere else, because the property is
#    write-only and cannot be read back (phase-1 finding 6).
if frontend_log | grep -q 'engine-switch trigger registered'; then
    echo "PASS: $(frontend_log | sed -n 's/.*engine-switch trigger registered: /trigger registered: /p' | tail -1)"
else
    echo "FAIL: the engine-switch trigger was not registered"
    frontend_log | grep -iE 'trigger|panel|shortcut'
    fails=$((fails + 1))
fi

setsid "$here/run-entry-client.sh" switcher > "$WORK/entry.out" 2>/dev/null &
entry_pid=$!
for _ in $(seq 1 120); do grep -q '^READY' "$WORK/entry.out" 2>/dev/null && break; sleep 0.1; done
grep -q '^READY' "$WORK/entry.out" || { echo "FAIL: entry client never reported READY"; exit 1; }
sleep 2
grep -q 'activate ' "$WORK/frontend.log" || {
    echo "FAIL: the frontend was never activated; is another input method bound?"
    exit 1
}
# 1b. The daemon had no engine and the frontend gave it the head of the cycle.
#     Checked through the daemon, not the log: what matters is what a context
#     gets, and the log line only says we asked.
check "the frontend chose xkb:us::eng for an engine-less daemon" "xkb:us::eng" "$(await_engine "xkb:us::eng")"
if ! frontend_log | grep -q 'ibus had no global engine; chose xkb:us::eng'; then
    echo "FAIL: the frontend did not report choosing an engine"
    frontend_log | grep -i engine
    fails=$((fails + 1))
fi

# 2. Forward. The daemon consumes the trigger inside ProcessKeyEvent and
#    answers handled, so no space is typed either — which the entry's contents
#    at the end of the run confirm.
inject down $CTRL down $ALT key $SPACE up $ALT up $CTRL sleep 500
check "Ctrl+Alt+Space switched to mozc-on" "mozc-on" "$(await_engine mozc-on)"

# 3. And round again, which is the part a one-shot toggle would also pass but a
#    cycle that lost track of the current engine would not.
inject down $CTRL down $ALT key $SPACE up $ALT up $CTRL sleep 500
check "pressing it again switched back" "xkb:us::eng" "$(await_engine "xkb:us::eng")"

# 4. Backward. With two engines it lands on the same place, so the assertion is
#    that the daemon reported the backward registration: the flag we wrote into
#    the keycode slot came back out of GlobalShortcutKeyResponded.
inject down $SHIFT down $CTRL down $ALT key $SPACE up $ALT up $CTRL up $SHIFT sleep 500
engine="$(await_engine mozc-on)"
if frontend_log | grep -q 'engine-switch trigger:.*(backward)'; then
    echo "PASS: Shift+Ctrl+Alt+Space reported the backward binding"
else
    echo "FAIL: the backward binding did not fire"
    frontend_log | grep -E 'engine-switch trigger:|GlobalShortcutKeyResponded'
    fails=$((fails + 1))
fi
check "the backward trigger also switched the engine" "mozc-on" "$engine"

# 5. The switch is real, not just bookkeeping: mozc only converts romaji if the
#    engine behind our context actually changed.
inject type konnnitiha sleep 800 key $ENTER sleep 800
check "mozc converted after the switch" "こんにちは" "$(entry_text)"

echo "logs in $WORK"
exit $((fails > 0))
