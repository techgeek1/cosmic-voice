#!/usr/bin/env bash
#
# Hand the seat's input-method slot from IBus to cosmic-voice.
#
#   scripts/cutover.sh [--yes]
#
# ATTENDED ONLY. Run this yourself, at the keyboard, with a second terminal or
# a spare VT open. It changes one line of one autostart file and then tells you
# what to do by hand; it does not stop, start or reconfigure anything running,
# because the moment the live IBus bridge lets go of the input-method slot is
# the moment a mistake costs you the keyboard.
#
# What it changes:
#
#   ~/.config/autostart/ibus-wayland.desktop
#     Exec=ibus start --type wayland   ->   Exec=ibus-daemon --xim --panel disable
#
# That is the whole persistent part of the cutover. `ibus start --type wayland`
# launches `ibus-ui-gtk3 --enable-wayland-im`, which is the process holding the
# slot, and so does a bare `ibus start`: with no `--type` it probes the
# compositor for zwp_input_method_manager_v2 and takes the Wayland route when
# it finds one (ibus 1.5.34 tools/main.vala, start_daemon_real), so on COSMIC
# the two are the same command. Running ibus-daemon itself, with the daemon
# arguments the bridge would have passed it, is the only form that cannot grow
# a bridge. ibus-daemon, mozc and the XIM server for XWayland clients all still
# run; cosmic-voice takes over the Wayland input-method role and the panel
# duties that came with it.
#
# `scripts/rollback.sh` puts it back.

set -euo pipefail

AUTOSTART="$HOME/.config/autostart/ibus-wayland.desktop"
BACKUP="$AUTOSTART.pre-multiplexer"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/cosmic-voice/config.ron"

say()  { printf '%s\n' "$*"; }
die()  { printf 'cutover: %s\n' "$*" >&2; exit 1; }

yes=false
[ "${1:-}" = "--yes" ] && yes=true

# --- Preconditions -----------------------------------------------------------

[ -n "${WAYLAND_DISPLAY:-}" ] || die "no WAYLAND_DISPLAY; run this from inside your COSMIC session"
[ -f "$AUTOSTART" ] || die "$AUTOSTART does not exist; nothing here starts IBus's Wayland bridge"

# Both `ibus start` forms are accepted: `--type wayland` is what the package
# installs, and a bare `ibus start` is what an earlier revision of this script
# wrote, believing it would not launch the bridge. It does.
if ! grep -Eq '^Exec=ibus start( --type wayland)?$' "$AUTOSTART"; then
    if grep -q '^Exec=ibus-daemon --xim --panel disable$' "$AUTOSTART"; then
        say "Already cut over: $AUTOSTART runs ibus-daemon directly."
    else
        die "$AUTOSTART has an Exec line I do not recognise; edit it by hand:
$(grep '^Exec=' "$AUTOSTART")"
    fi
fi

if [ -e "$BACKUP" ]; then
    die "$BACKUP already exists. Either the cutover was already done (run
rollback.sh first if you want to redo it) or something else wrote that file."
fi

# The state the applet will find. Reported rather than enforced: the user may
# be preparing the cutover before installing the new build.
bridge_pid="$(pgrep -f -- '--enable-wayland-im' | head -1 || true)"

say ""
say "=== cosmic-voice input-method cutover ==="
say ""
say "This machine right now:"
say "  autostart : $(grep '^Exec=' "$AUTOSTART")"
if [ -n "$bridge_pid" ]; then
    say "  IM slot   : held by ibus-ui-gtk3 --enable-wayland-im (pid $bridge_pid)"
else
    say "  IM slot   : no ibus-ui-gtk3 --enable-wayland-im is running"
fi
if [ -f "$CONFIG" ]; then
    say "  config    : $(grep -E '^\s*(input_method|bind_input_method)' "$CONFIG" || echo 'input_method not set (defaults to Off)')"
else
    say "  config    : $CONFIG does not exist yet"
fi
say ""
say "About to rewrite ONE line of $AUTOSTART,"
say "keeping a copy at $BACKUP."
say "Nothing running is touched. The steps you do by hand are printed after."
say ""

if ! $yes; then
    printf 'Proceed? [y/N] '
    read -r answer
    case "$answer" in
        y|Y|yes|YES) ;;
        *) die "nothing changed" ;;
    esac
fi

# --- The change --------------------------------------------------------------

cp -p "$AUTOSTART" "$BACKUP"
# In place, through a temp file, so an interrupted write cannot leave a
# truncated autostart entry behind.
sed -E 's|^Exec=ibus start( --type wayland)?$|Exec=ibus-daemon --xim --panel disable|' "$AUTOSTART" > "$AUTOSTART.tmp"
mv "$AUTOSTART.tmp" "$AUTOSTART"

say ""
say "Done: $(grep '^Exec=' "$AUTOSTART")   (backup in $BACKUP)"

# --- What is left, and it is yours -------------------------------------------

cat <<'STEPS'

=== Now, by hand, in this order ===

Keep a second terminal open, or know how to reach a VT (Ctrl+Alt+F3). If the
keyboard stops responding at any point, the recovery is at the bottom.

  1. Set the mode. In ~/.config/cosmic-voice/config.ron:

         input_method: Multiplexer,

     (If the file still says `bind_input_method: true`, replace that line;
     the old flag is ignored with a warning and does not turn anything on.)

  2. Retire IBus's Wayland bridge in the session that is already running.
     The autostart line only takes effect at the next login, so:

         ibus exit
         ibus-daemon --xim --panel disable --daemonize

     `ibus exit` stops ibus-daemon and the ibus-ui-gtk3 that holds the slot;
     the second line brings the daemon back on its own, so nothing rebinds
     it. Do NOT use `ibus start` for this, with or without `--type`: on a
     Wayland session it launches the bridge again. Typed CJK stops working at
     this point — that is expected, and it is what cosmic-voice is about to
     take over.

     Check that the slot really is free:

         pgrep -af -- --enable-wayland-im     # must print nothing

  3. Restart the applet, so it picks up the new config and binds the slot.
     Remove Voice from the panel and add it again through COSMIC's applet
     settings, or log out and back in.

  4. Check it. Open the applet popup:

       * "Ready — hold F13 to talk · Mozc あ"  — the multiplexer is running
         and IBus told it which engine is in effect.
       * "Input method: blocked — ibus-ui-gtk3 --enable-wayland-im is running"
         — step 2 did not take. Redo it; the applet notices within a few
         seconds and needs no restart.
       * "Input method: not running — …" — the frontend failed to start. The
         reason is on the line; `journalctl --user -f` while it retries.

     Then type into a text field: mozc must convert as before, and the
     candidate window is now cosmic-voice's rather than ibus-ui-gtk3's.
     Then hold the trigger and talk: the text arrives as preedit and is
     committed when you let go.

  5. Log out and back in once, to confirm the autostart change gives the same
     result from cold.

=== If the keyboard dies ===

This is what the whole design is built to avoid, and it has happened once. The
symptom is total: no window receives any key, including the panel.

  * Switch VT:            Ctrl+Alt+F3   (a text console, unaffected)
  * Log in there, then:   pkill -f ibus-ui-gtk3
                          pkill -x cosmic-voice
    Killing either holder releases the grab, because a keyboard grab is
    released by its object's destructor and the object dies with the process.
  * Back to the desktop:  Ctrl+Alt+F1  (or F2)
  * Undo the cutover:     scripts/rollback.sh

If Ctrl+Alt+F3 does not work either, ssh in from another machine and run the
same two pkills.

STEPS
