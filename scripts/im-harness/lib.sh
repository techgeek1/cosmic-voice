# Shared helpers for the input-method test harness.
# Source this; do not execute it.
#
# SAFETY: everything here is built so that no harness process can ever end up
# talking to the user's live Wayland session or live ibus-daemon.

set -euo pipefail

# All harness state lives here. It must be a SHORT path: AF_UNIX sun_path is
# 108 bytes and both the nested wayland socket and the private D-Bus socket
# live under $IM_HARNESS_STATE/runtime.
IM_HARNESS_STATE="${IM_HARNESS_STATE:-/tmp/im-harness-$(id -u)}"

HARNESS_RUNTIME="$IM_HARNESS_STATE/runtime"
HARNESS_HOME="$IM_HARNESS_STATE/home"
HARNESS_CONFIG="$IM_HARNESS_STATE/config"
HARNESS_DATA="$IM_HARNESS_STATE/data"
HARNESS_CACHE="$IM_HARNESS_STATE/cache"
HARNESS_STATE_HOME="$IM_HARNESS_STATE/state"
HARNESS_ENV_FILE="$IM_HARNESS_STATE/harness.env"

# The live session's socket, captured before we override anything.
LIVE_RUNTIME="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
LIVE_WAYLAND="${WAYLAND_DISPLAY:-}"

harness_die() { echo "im-harness: $*" >&2; exit 1; }
harness_log() { echo "im-harness: $*" >&2; }

harness_mkdirs() {
    mkdir -p "$HARNESS_RUNTIME" "$HARNESS_HOME" "$HARNESS_CONFIG" \
             "$HARNESS_DATA" "$HARNESS_CACHE" "$HARNESS_STATE_HOME"
    chmod 700 "$IM_HARNESS_STATE" "$HARNESS_RUNTIME"
}

# Absolute path of the live wayland socket, for the "never touch this" checks.
harness_live_socket() {
    [ -n "$LIVE_WAYLAND" ] || return 0
    case "$LIVE_WAYLAND" in
        /*) echo "$LIVE_WAYLAND" ;;
        *)  echo "$LIVE_RUNTIME/$LIVE_WAYLAND" ;;
    esac
}

# Hard refusal: never let a harness client point at the live compositor.
harness_assert_not_live() {
    local disp="$1" live
    live="$(harness_live_socket)"
    [ -n "$live" ] || return 0
    case "$disp" in
        /*) : ;;
        *) harness_die "internal error: display '$disp' is not an absolute socket path" ;;
    esac
    if [ "$(readlink -f "$disp" 2>/dev/null || echo "$disp")" = "$(readlink -f "$live" 2>/dev/null || echo "$live")" ]; then
        harness_die "REFUSING: '$disp' resolves to the LIVE session socket ($live)"
    fi
}

harness_require_env_file() {
    [ -r "$HARNESS_ENV_FILE" ] || \
        harness_die "no harness environment found; run scripts/im-harness/nested-comp.sh start first"
    # shellcheck disable=SC1090
    . "$HARNESS_ENV_FILE"
    harness_assert_not_live "$HARNESS_WAYLAND_DISPLAY"
}

# Kill a process group recorded in a pid file, then verify it is gone.
harness_kill_pgid_file() {
    local file="$1" label="$2" pgid
    [ -r "$file" ] || return 0
    pgid="$(cat "$file")"
    [ -n "$pgid" ] || { rm -f "$file"; return 0; }
    if kill -0 "-$pgid" 2>/dev/null; then
        harness_log "stopping $label (pgid $pgid)"
        kill -TERM -- "-$pgid" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "-$pgid" 2>/dev/null || break
            sleep 0.3
        done
        if kill -0 "-$pgid" 2>/dev/null; then
            harness_log "$label did not exit, sending KILL"
            kill -KILL -- "-$pgid" 2>/dev/null || true
            sleep 0.5
        fi
    fi
    if kill -0 "-$pgid" 2>/dev/null; then
        harness_log "WARNING: $label (pgid $pgid) still alive"
    else
        rm -f "$file"
    fi
}
