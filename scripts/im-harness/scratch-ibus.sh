#!/usr/bin/env bash
#
# Start a fully isolated ibus-daemon for the harness.
#
#   scratch-ibus.sh start          start the scratch daemon, print IBUS_ADDRESS
#   scratch-ibus.sh env            print the env clients should use
#   scratch-ibus.sh engine NAME    set the scratch daemon's GLOBAL engine
#   scratch-ibus.sh run CMD...     run a command in the scratch ibus env
#   scratch-ibus.sh status
#   scratch-ibus.sh stop           kill the daemon and every child
#
# Isolation, all of it load bearing:
#   * private D-Bus session bus with no service directories, so nothing here
#     can reach - or D-Bus-activate against - the live session
#   * HOME/XDG_{CONFIG,CACHE,DATA,STATE}_HOME under the harness state dir
#   * ~/.config/mozc and ~/.config/ibus are COPIED in; the originals are never
#     opened by anything this script starts
#   * an explicit --address, so the daemon's socket is in the harness tree and
#     the live daemon's address file is neither read nor written
#
# Never run `ibus restart`, `ibus exit` or `ibus engine` without IBUS_ADDRESS
# pointing at this daemon: without it those commands hit the LIVE daemon.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$here/lib.sh"

IBUS_ROOT="$IM_HARNESS_STATE/ibus"
IBUS_HOME="$IBUS_ROOT/home"
IBUS_CONFIG="$IBUS_ROOT/config"
IBUS_CACHE="$IBUS_ROOT/cache"
IBUS_DATA="$IBUS_ROOT/data"
IBUS_RUNTIME="$IBUS_ROOT/runtime"
IBUS_SOCKDIR="$IBUS_ROOT/sock"
IBUS_BUS_SOCKET="$IBUS_RUNTIME/dbus"
IBUS_BUS_CONF="$IBUS_ROOT/session-bus.conf"
IBUS_PGID_FILE="$IBUS_ROOT/ibus.pgid"
IBUS_BUSPG_FILE="$IBUS_ROOT/bus.pgid"
IBUS_ENV_FILE="$IBUS_ROOT/ibus.env"
IBUS_LOG="$IBUS_ROOT/ibus.log"

scratch_env() {
    # Deliberately no WAYLAND_DISPLAY and no DISPLAY: nothing started here
    # should be able to reach a compositor at all.
    #
    # Which is why MOZC_IBUS_CANDIDATE_WINDOW is set. mozc picks its candidate
    # window at engine startup: with WAYLAND_DISPLAY unset it decides it is on
    # X11 and drives its own `mozc_renderer`, emitting no lookup tables at all
    # — so a harness with no compositor in the engine's environment silently
    # tests the wrong routing. On the live session WAYLAND_DISPLAY *is* set and
    # COSMIC is not in `compatible_wayland_desktop_names` (["GNOME"]), so
    # candidates go down the IBus lookup-table path. This variable forces that
    # same path without handing the engine a compositor to reach.
    printf '%s\n' \
        "HOME=$IBUS_HOME" \
        "USER=${USER:-$(id -un)}" \
        "PATH=/usr/local/bin:/usr/bin" \
        "LANG=${LANG:-C.UTF-8}" \
        "XDG_RUNTIME_DIR=$IBUS_RUNTIME" \
        "XDG_CONFIG_HOME=$IBUS_CONFIG" \
        "XDG_CACHE_HOME=$IBUS_CACHE" \
        "XDG_DATA_HOME=$IBUS_DATA" \
        "XDG_STATE_HOME=$IBUS_ROOT/state" \
        "XDG_DATA_DIRS=/usr/local/share:/usr/share" \
        "XDG_CONFIG_DIRS=/etc/xdg" \
        "XDG_CURRENT_DESKTOP=COSMIC" \
        "MOZC_IBUS_CANDIDATE_WINDOW=ibus" \
        "DBUS_SESSION_BUS_ADDRESS=unix:path=$IBUS_BUS_SOCKET"
}

seed_config() {
    mkdir -p "$IBUS_HOME" "$IBUS_CONFIG" "$IBUS_CACHE" "$IBUS_DATA" \
             "$IBUS_RUNTIME" "$IBUS_SOCKDIR" "$IBUS_ROOT/state"
    chmod 700 "$IBUS_RUNTIME" "$IBUS_SOCKDIR"
    # Copies only. Never symlink these; a symlink would let the scratch mozc
    # engine write the user's real dictionary and history.
    if [ -d "$HOME/.config/mozc" ] && [ ! -d "$IBUS_CONFIG/mozc" ]; then
        cp -a "$HOME/.config/mozc" "$IBUS_CONFIG/mozc" 2>/dev/null || true
        # .session.ipc holds the abstract-socket name of the *running* mozc_server
        # and .server.lock is its singleton lock. Copying them would make the
        # scratch engine attach to the LIVE mozc_server and share the user's
        # dictionary and history. Drop both so a scratch server is spawned.
        rm -f "$IBUS_CONFIG/mozc/.server.lock" "$IBUS_CONFIG/mozc/.session.ipc"
        harness_log "copied ~/.config/mozc -> $IBUS_CONFIG/mozc"
    fi
    if [ -d "$HOME/.config/ibus" ] && [ ! -d "$IBUS_CONFIG/ibus" ]; then
        cp -a "$HOME/.config/ibus" "$IBUS_CONFIG/ibus" 2>/dev/null || true
        # Drop the copied address files; they point at the LIVE daemon.
        rm -rf "$IBUS_CONFIG/ibus/bus"
        harness_log "copied ~/.config/ibus -> $IBUS_CONFIG/ibus (address files dropped)"
    fi
}

start_bus() {
    if [ -S "$IBUS_BUS_SOCKET" ] && [ -r "$IBUS_BUSPG_FILE" ] && kill -0 "-$(cat "$IBUS_BUSPG_FILE")" 2>/dev/null; then
        return 0
    fi
    rm -f "$IBUS_BUS_SOCKET"
    cat > "$IBUS_BUS_CONF" <<XML
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$IBUS_BUS_SOCKET</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
XML
    setsid dbus-daemon --config-file="$IBUS_BUS_CONF" --nofork --nopidfile \
        > "$IBUS_ROOT/dbus.log" 2>&1 &
    echo $! > "$IBUS_BUSPG_FILE"
    for _ in $(seq 1 50); do [ -S "$IBUS_BUS_SOCKET" ] && break; sleep 0.1; done
    [ -S "$IBUS_BUS_SOCKET" ] || harness_die "scratch D-Bus bus failed to start"
}

address_file() { ls "$IBUS_CONFIG"/ibus/bus/* 2>/dev/null | head -n1 || true; }

start_daemon() {
    if [ -r "$IBUS_PGID_FILE" ] && kill -0 "-$(cat "$IBUS_PGID_FILE")" 2>/dev/null; then
        harness_die "scratch ibus-daemon already running (pgid $(cat "$IBUS_PGID_FILE")); use 'stop'"
    fi
    rm -rf "$IBUS_CONFIG/ibus/bus" "$IBUS_SOCKDIR"/*
    # --panel disable  : no ibus-ui-gtk3, so nothing binds zwp_input_method_v2
    # --config disable : no dconf config module, so the live dconf is not read
    #                    and cannot be written
    # (no -x)          : no XIM server
    # --address        : socket lives in the harness tree
    # shellcheck disable=SC2046
    # ibus-daemon exits when its parent process goes away, so it needs a
    # long-lived parent: the `sh -c ... & wait` keeps one alive inside the new
    # session, and killing the process group takes both down.
    # shellcheck disable=SC2046
    setsid env -i $(scratch_env) sh -c \
        'ibus-daemon --panel disable --config disable --emoji-extension disable \
                     --address "unix:tmpdir='"$IBUS_SOCKDIR"'" --cache refresh --verbose & wait' \
        > "$IBUS_LOG" 2>&1 &
    echo $! > "$IBUS_PGID_FILE"

    local f=""
    for _ in $(seq 1 100); do
        f="$(address_file)" || true
        [ -n "$f" ] && break
        kill -0 "-$(cat "$IBUS_PGID_FILE")" 2>/dev/null || { cat "$IBUS_LOG" >&2; harness_die "ibus-daemon exited during startup"; }
        sleep 0.1
    done
    [ -n "$f" ] || { cat "$IBUS_LOG" >&2; harness_die "no ibus address file appeared under $IBUS_CONFIG/ibus/bus"; }

    local addr pid
    addr="$(sed -n 's/^IBUS_ADDRESS=//p' "$f")"
    pid="$(sed -n 's/^IBUS_DAEMON_PID=//p' "$f")"
    case "$addr" in
        *"$IBUS_SOCKDIR"*) : ;;
        *) harness_die "REFUSING: scratch daemon reported an address outside the harness tree: $addr" ;;
    esac
    { scratch_env; echo "IBUS_ADDRESS=$addr"; } > "$IBUS_ENV_FILE"
    echo "IBUS_DAEMON_PID=$pid" >> "$IBUS_ENV_FILE"
    harness_log "scratch ibus-daemon pid $pid, pgid $(cat "$IBUS_PGID_FILE")"
    echo "IBUS_ADDRESS=$addr"
}

run_in_scratch() {
    [ -r "$IBUS_ENV_FILE" ] || harness_die "scratch ibus is not running; run 'start' first"
    # shellcheck disable=SC2046
    exec env -i $(cat "$IBUS_ENV_FILE" | grep -v '^IBUS_DAEMON_PID=') "$@"
}

case "${1:-start}" in
    start)
        harness_mkdirs
        seed_config
        start_bus
        start_daemon
        ;;
    env)
        [ -r "$IBUS_ENV_FILE" ] || harness_die "scratch ibus is not running"
        sed 's/^/export /' "$IBUS_ENV_FILE"
        ;;
    engine)
        # `ibus engine <name>` exits 1 even when it succeeds, so read it back.
        [ -n "${2:-}" ] || harness_die "usage: $0 engine <name>"
        "$0" run ibus engine "$2" || true
        sleep 0.5
        got="$("$0" run ibus engine 2>/dev/null || true)"
        [ "$got" = "$2" ] || harness_die "global engine is '$got', expected '$2'"
        echo "global engine: $got"
        ;;
    run)
        shift
        run_in_scratch "$@"
        ;;
    status)
        for f in "$IBUS_BUSPG_FILE:scratch-bus" "$IBUS_PGID_FILE:scratch-ibus"; do
            file="${f%%:*}"; label="${f##*:}"
            if [ -r "$file" ] && kill -0 "-$(cat "$file")" 2>/dev/null; then
                echo "$label: running (pgid $(cat "$file"))"
            else
                echo "$label: not running"
            fi
        done
        [ -r "$IBUS_ENV_FILE" ] && grep -E '^IBUS_' "$IBUS_ENV_FILE"
        echo "--- processes in the scratch tree ---"
        ps -u "$(id -u)" -o pid,ppid,cmd | grep -E "ibus|mozc" | grep -v grep || true
        ;;
    stop)
        harness_kill_pgid_file "$IBUS_PGID_FILE" "scratch ibus-daemon"
        harness_kill_pgid_file "$IBUS_BUSPG_FILE" "scratch ibus D-Bus bus"
        rm -f "$IBUS_ENV_FILE" "$IBUS_BUS_SOCKET"
        # Anything the daemon spawned lives in the scratch tree; catch strays.
        pkill -f "$IBUS_ROOT" 2>/dev/null || true
        # mozc_server daemonises and its argv says nothing about the profile,
        # so it outlives its engine and has to be matched on its environment.
        for p in $(pgrep -u "$(id -u)" -x mozc_server 2>/dev/null || true); do
            if tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -q "^HOME=$IBUS_HOME$"; then
                harness_log "stopping scratch mozc_server (pid $p)"
                kill -TERM "$p" 2>/dev/null || true
            fi
        done
        sleep 0.5
        left="$(ps -u "$(id -u)" -o pid,cmd | grep -F "$IBUS_ROOT" | grep -v grep || true)"
        for p in $(pgrep -u "$(id -u)" -x mozc_server 2>/dev/null || true); do
            tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -q "^HOME=$IBUS_HOME$" \
                && left="$left
scratch mozc_server still running: pid $p"
        done
        if [ -n "$left" ]; then echo "WARNING: still running:"; echo "$left"; else echo "im-harness: scratch ibus stopped"; fi
        ;;
    *) harness_die "usage: $0 {start|env|engine NAME|run CMD...|status|stop}" ;;
esac
