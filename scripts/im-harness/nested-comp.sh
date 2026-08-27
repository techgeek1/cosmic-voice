#!/usr/bin/env bash
#
# Start /usr/bin/cosmic-comp NESTED (winit backend) inside the live session,
# on a private D-Bus session bus and a private XDG_RUNTIME_DIR.
#
# Usage:
#   nested-comp.sh start     start the private bus + nested compositor
#   nested-comp.sh env       print the environment clients should use
#   nested-comp.sh status    show what is running
#   nested-comp.sh log       tail the compositor log
#   nested-comp.sh stop      kill everything this script started
#
# Why a private bus (do not "simplify" this away):
#   * cosmic-comp calls conn.request_name("com.system76.CosmicComp") on the
#     session bus. On the live bus that races with / replaces the live
#     compositor's name, which would break the live portal and OSK EI paths.
#   * The com.system76.CosmicComp.Ei interface (the only way to get an EIS
#     socket out of cosmic-comp) is addressed by that well-known name, so the
#     harness must reach *our* compositor's copy of it.
#   * The private bus is configured with no service directories, so nothing can
#     be D-Bus-activated on it (in particular org.freedesktop.IBus, which would
#     otherwise spawn a daemon against the user's real HOME).
#
# The compositor opens a normal window on the live desktop. That is expected.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$here/lib.sh"

COMP_BIN="${COMP_BIN:-/usr/bin/cosmic-comp}"
COMP_LOG="$IM_HARNESS_STATE/comp.log"
COMP_PGID_FILE="$IM_HARNESS_STATE/comp.pgid"
BUS_PGID_FILE="$IM_HARNESS_STATE/bus.pgid"
BUS_CONF="$IM_HARNESS_STATE/session-bus.conf"
BUS_SOCKET="$HARNESS_RUNTIME/dbus"

write_bus_conf() {
    cat > "$BUS_CONF" <<XML
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$BUS_SOCKET</listen>
  <!-- deliberately no <servicedir>: nothing may be D-Bus activated here -->
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
  <limit name="max_incoming_unix_fds">64</limit>
  <limit name="max_outgoing_unix_fds">64</limit>
</busconfig>
XML
}

start_bus() {
    if [ -S "$BUS_SOCKET" ] && [ -r "$BUS_PGID_FILE" ] && kill -0 "-$(cat "$BUS_PGID_FILE")" 2>/dev/null; then
        harness_log "private bus already running"
        return 0
    fi
    rm -f "$BUS_SOCKET"
    write_bus_conf
    setsid dbus-daemon --config-file="$BUS_CONF" --nofork --nopidfile \
        > "$IM_HARNESS_STATE/dbus.log" 2>&1 &
    echo $! > "$BUS_PGID_FILE"
    for _ in $(seq 1 50); do [ -S "$BUS_SOCKET" ] && break; sleep 0.1; done
    [ -S "$BUS_SOCKET" ] || harness_die "private D-Bus session bus failed to start; see $IM_HARNESS_STATE/dbus.log"
    harness_log "private bus at unix:path=$BUS_SOCKET"
}

find_nested_socket() {
    # The private runtime dir starts out with only 'dbus' and the
    # 'host-wayland' symlink, so any wayland-N socket in it is the nested one.
    local s
    for s in "$HARNESS_RUNTIME"/wayland-*; do
        case "$s" in *.lock) continue ;; esac
        [ -S "$s" ] && { echo "$s"; return 0; }
    done
    return 1
}

start_comp() {
    if [ -r "$COMP_PGID_FILE" ] && kill -0 "-$(cat "$COMP_PGID_FILE")" 2>/dev/null; then
        harness_die "nested compositor already running (pgid $(cat "$COMP_PGID_FILE")); use 'stop' first"
    fi
    [ -n "$LIVE_WAYLAND" ] || harness_die "no WAYLAND_DISPLAY in the environment; the nested compositor needs a host session"
    [ -x "$COMP_BIN" ] || harness_die "$COMP_BIN not executable"

    # Point the compositor at the live socket through a symlink with a name
    # that cannot be confused with a nested display.
    ln -sfn "$(harness_live_socket)" "$HARNESS_RUNTIME/host-wayland"
    rm -f "$HARNESS_RUNTIME"/wayland-* 2>/dev/null || true

    # env -i style: only what the compositor needs. Notably absent:
    #   COSMIC_SESSION_SOCK  - would make it talk to the live cosmic-session
    #   DISPLAY              - would select the x11 backend
    #   the user's XDG_*_HOME and HOME
    setsid env -i \
        HOME="$HARNESS_HOME" \
        USER="${USER:-$(id -un)}" \
        PATH="/usr/local/bin:/usr/bin" \
        LANG="${LANG:-C.UTF-8}" \
        XDG_RUNTIME_DIR="$HARNESS_RUNTIME" \
        XDG_CONFIG_HOME="$HARNESS_CONFIG" \
        XDG_DATA_HOME="$HARNESS_DATA" \
        XDG_CACHE_HOME="$HARNESS_CACHE" \
        XDG_STATE_HOME="$HARNESS_STATE_HOME" \
        XDG_DATA_DIRS="/usr/local/share:/usr/share" \
        XDG_CONFIG_DIRS="/etc/xdg" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=$BUS_SOCKET" \
        WAYLAND_DISPLAY="host-wayland" \
        COSMIC_BACKEND="winit" \
        COSMIC_ENFORCE_DBUS_OWNERS="0" \
        RUST_LOG="${COMP_RUST_LOG:-info}" \
        RUST_BACKTRACE=1 \
        "$COMP_BIN" --no-xwayland > "$COMP_LOG" 2>&1 &
    echo $! > "$COMP_PGID_FILE"

    local sock=""
    for _ in $(seq 1 100); do
        sock="$(find_nested_socket || true)"
        [ -n "$sock" ] && break
        kill -0 "-$(cat "$COMP_PGID_FILE")" 2>/dev/null || \
            { cat "$COMP_LOG" >&2; harness_die "compositor exited during startup"; }
        sleep 0.1
    done
    [ -n "$sock" ] || { cat "$COMP_LOG" >&2; harness_die "no nested wayland socket appeared in $HARNESS_RUNTIME"; }

    harness_assert_not_live "$sock"

    cat > "$HARNESS_ENV_FILE" <<ENV
# Generated by nested-comp.sh. Source this to talk to the NESTED session.
HARNESS_STATE="$IM_HARNESS_STATE"
HARNESS_RUNTIME="$HARNESS_RUNTIME"
# Absolute socket path on purpose: libwayland accepts an absolute
# WAYLAND_DISPLAY, and it removes any chance of a bare "wayland-1" being
# resolved against the live \$XDG_RUNTIME_DIR.
HARNESS_WAYLAND_DISPLAY="$sock"
HARNESS_DBUS_ADDRESS="unix:path=$BUS_SOCKET"
HARNESS_COMP_LOG="$COMP_LOG"
ENV
    harness_log "nested compositor pgid $(cat "$COMP_PGID_FILE"), socket $sock"
}

case "${1:-start}" in
    start)
        harness_mkdirs
        start_bus
        start_comp
        echo "# eval \"\$($here/nested-comp.sh env)\""
        "$0" env
        ;;
    env)
        harness_require_env_file
        echo "export XDG_RUNTIME_DIR='$HARNESS_RUNTIME'"
        echo "export WAYLAND_DISPLAY='$HARNESS_WAYLAND_DISPLAY'"
        echo "export DBUS_SESSION_BUS_ADDRESS='$HARNESS_DBUS_ADDRESS'"
        ;;
    status)
        for f in "$BUS_PGID_FILE:private-bus" "$COMP_PGID_FILE:nested-comp"; do
            file="${f%%:*}"; label="${f##*:}"
            if [ -r "$file" ] && kill -0 "-$(cat "$file")" 2>/dev/null; then
                echo "$label: running (pgid $(cat "$file"))"
            else
                echo "$label: not running"
            fi
        done
        [ -r "$HARNESS_ENV_FILE" ] && cat "$HARNESS_ENV_FILE"
        ;;
    log)  tail -n "${2:-50}" "$COMP_LOG" ;;
    stop)
        harness_kill_pgid_file "$COMP_PGID_FILE" "nested compositor"
        harness_kill_pgid_file "$BUS_PGID_FILE" "private D-Bus bus"
        rm -f "$HARNESS_ENV_FILE" "$BUS_SOCKET" "$HARNESS_RUNTIME"/wayland-*
        ps -u "$(id -u)" -o pid,cmd | grep -E "cosmic-comp --no-xwayland|dbus-daemon --config-file=$BUS_CONF" | grep -v grep || echo "im-harness: nothing left running"
        ;;
    *) harness_die "usage: $0 {start|env|status|log|stop}" ;;
esac
