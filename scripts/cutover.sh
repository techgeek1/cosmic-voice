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
# The copy it keeps goes under $XDG_STATE_HOME/cosmic-voice, NOT beside the
# original. systemd-xdg-autostart-generator enumerates every file in
# ~/.config/autostart, not just the ones ending in .desktop, so a backup kept in
# that directory is launched exactly like the entry it backs up: the cutover
# holds until the next login and then quietly undoes itself, because the saved
# `Exec=ibus start --type wayland` comes back as a second unit and takes the
# input-method slot again. An earlier revision of this script did that; if it
# left a backup on this machine, the run below moves it out of the way.
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

AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
AUTOSTART="$AUTOSTART_DIR/ibus-wayland.desktop"
# Outside $AUTOSTART_DIR on purpose — see the note at the top of the file.
BACKUP_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/cosmic-voice"
BACKUP="$BACKUP_DIR/ibus-wayland.desktop.pre-multiplexer"
# Where revisions of this script up to 2026-08 put it, which is the bug.
LEGACY_BACKUP="$AUTOSTART.pre-multiplexer"
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
#
# Re-running on an already cut-over machine is not an error and does not
# rewrite anything: it is how you get the sweep below, which is the part that
# repairs a machine the old backup location broke.
cut_over=false
if grep -q '^Exec=ibus-daemon --xim --panel disable$' "$AUTOSTART"; then
    cut_over=true
elif ! grep -Eq '^Exec=ibus start( --type wayland)?$' "$AUTOSTART"; then
    die "$AUTOSTART has an Exec line I do not recognise; edit it by hand:
$(grep '^Exec=' "$AUTOSTART")"
fi

# --- Nothing else in the autostart directory may start the bridge ------------

# The generator does not filter on the .desktop suffix, so every regular file
# here is a potential autostart entry. Our own old backup is one we know how to
# fix; anything else is the user's and only they know what it is for.
if [ -f "$LEGACY_BACKUP" ]; then
    [ -e "$BACKUP" ] && die "two backups: $LEGACY_BACKUP and $BACKUP.
The first one is autostarted and has to go. Compare them and delete whichever
is not the pre-cutover entry, then run this again."
    mkdir -p "$BACKUP_DIR"
    mv "$LEGACY_BACKUP" "$BACKUP"
    say ""
    say "Moved a backup out of the autostart directory:"
    say "  from : $LEGACY_BACKUP   (was being autostarted, which relaunched"
    say "                           IBus's bridge at every login)"
    say "  to   : $BACKUP"
fi

strays=""
for entry in "$AUTOSTART_DIR"/*; do
    [ -f "$entry" ] || continue
    [ "$entry" = "$AUTOSTART" ] && continue
    if grep -Eq '^Exec=.*(ibus start|--enable-wayland-im)' "$entry" 2>/dev/null; then
        strays="$strays  $entry: $(grep -m1 '^Exec=' "$entry")
"
    fi
done
[ -n "$strays" ] && die "these files in $AUTOSTART_DIR also start IBus's Wayland
bridge, and every one of them is autostarted regardless of its extension:
$strays
Move them somewhere outside that directory (or delete them) and run this again."

if [ -e "$BACKUP" ] && ! $cut_over; then
    die "$BACKUP already exists but $AUTOSTART is not cut over.
Run rollback.sh first if you want to redo the cutover, or delete that file if
you know it is stale."
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

# --- The change --------------------------------------------------------------

if $cut_over; then
    say "The autostart entry is already cut over; nothing to rewrite."
    say "The steps you do by hand are printed below — do them if the live"
    say "session still has a bridge running."
else
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

    mkdir -p "$BACKUP_DIR"
    cp -p "$AUTOSTART" "$BACKUP"
    # In place, through a temp file, so an interrupted write cannot leave a
    # truncated autostart entry behind. The temp has to be in $AUTOSTART_DIR for
    # the rename to be atomic — a different filesystem would make it a
    # copy-and-unlink, which is the very thing being guarded against — so the
    # trap is what keeps an interrupted run from leaving an autostartable file
    # behind instead.
    tmp="$AUTOSTART.tmp"
    trap 'rm -f "$tmp"' EXIT
    sed -E 's|^Exec=ibus start( --type wayland)?$|Exec=ibus-daemon --xim --panel disable|' "$AUTOSTART" > "$tmp"
    mv "$tmp" "$AUTOSTART"

    say ""
    say "Done: $(grep '^Exec=' "$AUTOSTART")   (backup in $BACKUP)"
fi

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

     `ibus exit` talks to ibus-daemon, and the second line brings the daemon
     back on its own so nothing rebinds the slot. Do NOT use `ibus start` for
     this, with or without `--type`: on a Wayland session it launches the
     bridge again. Typed CJK stops working at this point — that is expected,
     and it is what cosmic-voice is about to take over.

     Check that the slot really is free:

         pgrep -af -- --enable-wayland-im     # must print nothing

     `ibus exit` does NOT reliably take the bridge with it. It stops the
     daemon it can reach over the bus; an ibus-ui-gtk3 started as its own
     autostart unit outlives that and goes on holding the slot with no daemon
     behind it, which is the worst of both. If the pgrep above still prints a
     process, kill it by pid and re-run the pgrep:

         kill <pid>
         pgrep -af -- --enable-wayland-im     # now must print nothing

     Do this before the `ibus-daemon` line above if you can; if you already
     ran it, just kill the bridge and carry on. The applet binds within five
     seconds either way.

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
