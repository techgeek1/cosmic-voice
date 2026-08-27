#!/usr/bin/env bash
#
# Run entry-client.py against the NESTED compositor.
#
#   run-entry-client.sh [title]
#   WAYLAND_DEBUG=1 run-entry-client.sh   # protocol trace on stderr
#
# GTK_IM_MODULE=wayland is load bearing: it selects gtk3's im-wayland.so, which
# is the zwp_text_input_v3 implementation. With GTK_IM_MODULE=ibus the entry
# would talk to an ibus daemon over D-Bus and never touch text-input-v3, which
# is exactly the path we are trying to exercise.

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$here/lib.sh"
harness_require_env_file

exec env -i \
    HOME="$HARNESS_HOME" \
    USER="${USER:-$(id -un)}" \
    PATH="/usr/local/bin:/usr/bin" \
    LANG="${LANG:-C.UTF-8}" \
    XDG_RUNTIME_DIR="$HARNESS_RUNTIME" \
    XDG_CONFIG_HOME="$HARNESS_CONFIG" \
    XDG_DATA_HOME="$HARNESS_DATA" \
    XDG_CACHE_HOME="$HARNESS_CACHE" \
    XDG_DATA_DIRS="/usr/local/share:/usr/share" \
    XDG_CONFIG_DIRS="/etc/xdg" \
    DBUS_SESSION_BUS_ADDRESS="$HARNESS_DBUS_ADDRESS" \
    WAYLAND_DISPLAY="$HARNESS_WAYLAND_DISPLAY" \
    GDK_BACKEND=wayland \
    GTK_IM_MODULE=wayland \
    ${WAYLAND_DEBUG:+WAYLAND_DEBUG="$WAYLAND_DEBUG"} \
    ${PYTHONUNBUFFERED:+PYTHONUNBUFFERED="$PYTHONUNBUFFERED"} \
    python3 "$here/entry-client.py" "$@"
