#!/usr/bin/env bash
#
# End-to-end check of the phase-6 panel presence: the engine's status menu and
# mode glyph, delivered to our own input context instead of a panel service.
#
#   scripts/im-harness/property-test.sh
#
# Same scaffolding as dictation-test.sh — nested cosmic-comp, isolated
# ibus-daemon with mozc, a GTK entry, keys over libei, and the control fifo
# `devtest im-frontend --control-fifo` reads ImCmds from — plus the config
# override switcher-test.sh uses, so the engine cycle is a known two engines.
# The frontend runs at debug level because the evidence is in its own log:
# there is no applet in the loop, so what is asserted is what the frontend
# would have published to one. See README.md, "Safety invariants".
#
# What it proves, in order:
#
#   1. registration     mozc registers its properties on FocusIn and they
#                       arrive on OUR context, decoded, with an InputMode menu
#   2. the glyph        the indicator is mozc's hiragana glyph, taken from the
#                       InputMode property's symbol via the engine's
#                       icon_prop_key
#   3. activation       ActivateProperty InputMode.Direct reaches mozc, which
#                       answers with UpdateProperty, and the indicator changes
#   4. it is real       konnnitiha now arrives as ASCII: direct mode
#   5. and back         InputMode.Hiragana converts again
#   6. engine change    SetEngine xkb:us::eng empties the menu and the glyph
#   7. re-registration  SetEngine mozc-on brings them back
#
# NEVER use `pkill -f` in here: this script's own command line contains every
# pattern you would want to match, and the user's cosmic-voice applet is called
# `cosmic-voice` too. Everything is killed by recorded pid.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
. "$here/lib.sh"
set +e

BIN="$repo/target/debug/cosmic-voice"
WORK="$IM_HARNESS_STATE/property-test"
FIFO="$WORK/control.fifo"
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

# The frontend's log with the tracing colour escapes taken out.
frontend_log() { sed 's/\x1b\[[0-9;]*m//g' "$WORK/frontend.log"; }

# The last committed contents of the entry, and the last preedit it was shown.
entry_text()   { sed -n 's/^ENTRY //p'   "$WORK/entry.out" | tail -1; }
preedit_text() { sed -n 's/^PREEDIT //p' "$WORK/entry.out" | tail -1; }

# The indicator the frontend last published, or "-" for none. The line is
# `ibus properties: indicator あ, 2 menu entries`.
indicator() { frontend_log | sed -n 's/.*properties: indicator \(.*\), [0-9]* menu entr.*/\1/p' | tail -1; }
menu_size() { frontend_log | sed -n 's/.*properties: indicator .*, \([0-9]*\) menu entr.*/\1/p' | tail -1; }

# Waits up to three seconds for the published indicator to become `want`.
# Activation is a method call out, an UpdateProperty burst back and a
# snapshot, all asynchronous, so this polls rather than sleeps.
await_indicator() {
    local want="$1" got=""
    for _ in $(seq 1 30); do
        got="$(indicator)"
        [ "$got" = "$want" ] && break
        sleep 0.1
    done
    echo "$got"
}

inject() { "$here/ei-inject" --bus "$HARNESS_DBUS_ADDRESS" "$@" >> "$WORK/inject.log" 2>&1; }

# One command down the fifo. Each write is its own open/write/close, which is
# what the reader's reopen loop is built for.
control() { printf '%s\n' "$1" > "$FIFO"; sleep 0.4; }

[ -x "$BIN" ] || { echo "build first: cargo build"; exit 2; }
[ -x "$here/ei-inject" ] || "$here/build-ei-inject.sh" || exit 2

"$here/nested-comp.sh" env >/dev/null 2>&1 || "$here/nested-comp.sh" start >/dev/null
harness_require_env_file
"$here/scratch-ibus.sh" start >/dev/null 2>&1
"$here/scratch-ibus.sh" engine mozc-on >/dev/null 2>&1
ibus_address="$(sed -n 's/^IBUS_ADDRESS=//p' "$IM_HARNESS_STATE/ibus/ibus.env")"
[ -n "$ibus_address" ] || { echo "no scratch ibus address"; exit 2; }

# A known cycle, so SetEngine has somewhere to go and the switcher describes
# both engines (which is where icon_prop_key comes from). Same values as
# switcher-test.sh, for the same reason.
cat > "$WORK/config.ron" <<RON
(
    ibus_triggers: ["<Control><Alt>space"],
    ibus_engines:  ["xkb:us::eng", "mozc-on"],
)
RON

mkfifo "$FIFO" || { echo "could not create $FIFO"; exit 2; }

RUST_LOG=cosmic_voice=debug setsid "$BIN" devtest im-frontend "$HARNESS_WAYLAND_DISPLAY" "$ibus_address" \
    --config "$WORK/config.ron" \
    --control-fifo "$FIFO" \
    > "$WORK/frontend.log" 2>&1 &
frontend_pid=$!
sleep 2
grep -q 'input method bound' "$WORK/frontend.log" || {
    echo "FAIL: the frontend did not bind the input method"
    frontend_log
    exit 1
}

setsid "$here/run-entry-client.sh" property > "$WORK/entry.out" 2>/dev/null &
entry_pid=$!
for _ in $(seq 1 120); do grep -q '^READY' "$WORK/entry.out" 2>/dev/null && break; sleep 0.1; done
grep -q '^READY' "$WORK/entry.out" || { echo "FAIL: entry client never reported READY"; exit 1; }
sleep 2
grep -q 'activate ' "$WORK/frontend.log" || {
    echo "FAIL: the frontend was never activated; is another input method bound?"
    exit 1
}

# 1. mozc registered on FocusIn and the signal came to us, decoded. The
#    capability bit is the whole mechanism: without CAP_PROPERTY this would
#    have gone to a panel service that does not exist.
# The decoded list prints one entry per line under the signal line.
if frontend_log | grep -q 'signal RegisterProperties [1-9]' && frontend_log | grep -q '^    InputMode menu '; then
    echo "PASS: mozc registered an InputMode menu on our context"
else
    echo "FAIL: no RegisterProperties with an InputMode menu arrived"
    frontend_log | grep -iE 'propert' | head
    fails=$((fails + 1))
fi

# 2. The glyph is the InputMode property's symbol: mozc-on starts in
#    hiragana, so あ.
check "the indicator is mozc's hiragana glyph" "あ" "$(await_indicator "あ")"
check "the menu has mozc's two top-level entries" "2" "$(menu_size)"

# 3. Activating the direct-input radio. mozc answers with a burst of
#    UpdateProperty — one per radio child, then the menu with its new symbol —
#    and the indicator follows the last of them.
control '{"ActivateProperty":{"key":"InputMode.Direct","state":1}}'
check "InputMode.Direct changed the indicator" "A" "$(await_indicator "A")"
if frontend_log | grep -q 'signal UpdateProperty InputMode.Direct radio .*checked'; then
    echo "PASS: mozc updated the radio child as checked"
else
    echo "FAIL: no UpdateProperty for InputMode.Direct arrived checked"
    frontend_log | grep 'UpdateProperty' | tail -5
    fails=$((fails + 1))
fi

# 4. Direct mode is real: romaji arrives as ASCII, uncommitted by mozc and
#    committed as text by our own routing rule 3.
inject type konnnitiha sleep 1000
check "in direct mode konnnitiha arrives as ASCII" "konnnitiha" "$(entry_text)"

# 5. And back to hiragana, which converts again.
control '{"ActivateProperty":{"key":"InputMode.Hiragana","state":1}}'
check "InputMode.Hiragana restored the indicator" "あ" "$(await_indicator "あ")"
inject type aiueo sleep 1000
check "hiragana converts again" "あいうえお" "$(preedit_text)"
inject key $ENTER sleep 800
check "and commits into the field" "konnnitihaあいうえお" "$(entry_text)"

# 6. Switching to an xkb engine, which registers nothing. With the field
#    active the daemon empties the list on the context stream when it unsets
#    mozc; the glyph goes with it because xkb:us::eng has no icon_prop_key.
control '{"SetEngine":"xkb:us::eng"}'
check "SetEngine xkb:us::eng cleared the indicator" "-" "$(await_indicator "-")"
check "and the menu" "0" "$(menu_size)"

# 7. Back to mozc: the daemon attaches it to the focused context, mozc
#    re-registers on FocusIn, and the glyph is hiragana again.
control '{"SetEngine":"mozc-on"}'
check "SetEngine mozc-on re-registered the menu" "あ" "$(await_indicator "あ")"
check "with both entries" "2" "$(menu_size)"

echo "logs in $WORK"
exit $((fails > 0))
