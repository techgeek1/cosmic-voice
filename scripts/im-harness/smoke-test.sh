#!/usr/bin/env bash
#
# End-to-end automated check of the harness: nested compositor + libei key
# injection + a text-input-v3 client. No live-session component is touched and
# no human input is needed.
#
#   scripts/im-harness/smoke-test.sh
#
# Passes when the injected keystrokes come out of the GTK entry AND the entry's
# zwp_text_input_v3 object got its `enter` event (which only happens when an
# input method is present - here the EI connection standing in for one).

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$here/lib.sh"

TEXT="${1:-hello world}"
WORK="$IM_HARNESS_STATE/smoke"
mkdir -p "$WORK"
rm -f "$WORK"/*

started_comp=0
hold_pgid=""
entry_pgid=""

cleanup() {
    [ -n "$entry_pgid" ] && kill -TERM -- "-$entry_pgid" 2>/dev/null || true
    [ -n "$hold_pgid" ] && kill -TERM -- "-$hold_pgid" 2>/dev/null || true
    sleep 0.5
    [ "$started_comp" = 1 ] && "$here/nested-comp.sh" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

[ -x "$here/ei-inject" ] || "$here/build-ei-inject.sh"

if ! "$here/nested-comp.sh" env >/dev/null 2>&1; then
    "$here/nested-comp.sh" start >/dev/null
    started_comp=1
fi
harness_require_env_file

# 1. Hold an EI keyboard connection open. cosmic-comp treats an active EI
#    keyboard connection as the input method when no zwp_input_method_v2 is
#    bound, which is what makes the compositor send text_input.enter.
setsid "$here/ei-inject" --bus "$HARNESS_DBUS_ADDRESS" hold 60000 \
    > "$WORK/hold.log" 2>&1 &
hold_pgid=$!
sleep 1.5

# 2. Start the entry client with a protocol trace.
setsid env WAYLAND_DEBUG=1 "$here/run-entry-client.sh" smoke \
    > "$WORK/entry.out" 2> "$WORK/entry.dbg" &
entry_pgid=$!
for _ in $(seq 1 100); do grep -q '^READY' "$WORK/entry.out" 2>/dev/null && break; sleep 0.1; done
grep -q '^READY' "$WORK/entry.out" || { echo "FAIL: entry client never reported READY"; exit 1; }

# 3. Inject.
"$here/ei-inject" --bus "$HARNESS_DBUS_ADDRESS" type "$TEXT" sleep 500 >> "$WORK/hold.log" 2>&1
sleep 0.5

fail=0
if grep -qF "ENTRY $TEXT" "$WORK/entry.out"; then
    echo "PASS: entry received '$TEXT' from libei injection"
else
    echo "FAIL: entry never showed '$TEXT'"; sed -n '1,40p' "$WORK/entry.out"; fail=1
fi
if grep -q 'zwp_text_input_v3#[0-9]*\.enter' "$WORK/entry.dbg"; then
    echo "PASS: client got zwp_text_input_v3.enter"
else
    echo "FAIL: no zwp_text_input_v3.enter (no input method visible to the compositor?)"; fail=1
fi
if grep -q '> zwp_text_input_v3#[0-9]*\.enable' "$WORK/entry.dbg"; then
    echo "PASS: client sent zwp_text_input_v3.enable"
else
    echo "FAIL: client never enabled text-input-v3"; fail=1
fi
echo "logs in $WORK"
exit $fail
