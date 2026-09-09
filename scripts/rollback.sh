#!/usr/bin/env bash
#
# Give the seat's input-method slot back to IBus.
#
#   scripts/rollback.sh [--yes]
#
# ATTENDED ONLY, like the cutover it reverses. Restores the autostart file from
# the backup `cutover.sh` left, and then tells you what to do by hand. It stops
# and starts nothing, for the same reason: the dangerous instant is the handover
# itself, and it should happen while you are looking at it.
#
# The rollback is genuinely one line — that is the point of doing the cutover
# this way. Everything else cosmic-voice's multiplexer does lives inside its own
# process and stops when you set `input_method: Off`.

set -euo pipefail

AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
AUTOSTART="$AUTOSTART_DIR/ibus-wayland.desktop"
BACKUP="${XDG_STATE_HOME:-$HOME/.local/state}/cosmic-voice/ibus-wayland.desktop.pre-multiplexer"
# Where revisions of this script up to 2026-08 left it: inside the autostart
# directory, where the generator picks it up as an entry of its own. Restoring
# from it is also what removes it, which is the point.
LEGACY_BACKUP="$AUTOSTART.pre-multiplexer"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/cosmic-voice/config.ron"

say() { printf '%s\n' "$*"; }
die() { printf 'rollback: %s\n' "$*" >&2; exit 1; }

yes=false
[ "${1:-}" = "--yes" ] && yes=true

if [ -f "$BACKUP" ]; then
    restore_from="$BACKUP"
elif [ -f "$LEGACY_BACKUP" ]; then
    restore_from="$LEGACY_BACKUP"
else
    die "no backup at $BACKUP.
If you never ran cutover.sh, there is nothing to roll back: set
\`input_method: Off\` in $CONFIG and restart the applet.
Otherwise write the autostart entry back by hand:
    Exec=ibus start --type wayland"
fi

say ""
say "=== cosmic-voice input-method rollback ==="
say ""
say "  now    : $(grep '^Exec=' "$AUTOSTART" 2>/dev/null || echo '(file missing)')"
say "  backup : $(grep '^Exec=' "$restore_from")   ($restore_from)"
say ""
say "About to restore $AUTOSTART from the backup."
say "Nothing running is touched."
say ""

if ! $yes; then
    printf 'Proceed? [y/N] '
    read -r answer
    case "$answer" in
        y|Y|yes|YES) ;;
        *) die "nothing changed" ;;
    esac
fi

mv "$restore_from" "$AUTOSTART"
say ""
say "Done: $(grep '^Exec=' "$AUTOSTART")"

cat <<'STEPS'

=== Now, by hand, in this order ===

The order is the reverse of the cutover's, and it matters: cosmic-voice has to
let the slot go before IBus's bridge tries to take it. Two binders on one seat
is the failure this whole subsystem exists to avoid.

  1. Stop the multiplexer first. In ~/.config/cosmic-voice/config.ron:

         input_method: Off,

     Then restart the applet — remove Voice from the panel and add it back
     through COSMIC's applet settings, or log out and in. Confirm nothing of
     ours holds the slot:

         pgrep -x cosmic-voice        # panel applets may still be listed;
                                      # the popup must say nothing about an
                                      # input method

  2. Only then, bring IBus's bridge back in the running session:

         ibus exit
         ibus start --type wayland

     Check it took:

         pgrep -af -- --enable-wayland-im     # must print one process now

  3. Type into a text field. mozc converts, and the candidate window is
     ibus-ui-gtk3's again. Dictation still works, on the virtual-keyboard
     path — that is what `input_method: Off` has always meant.

  4. Log out and back in once, to confirm the restored autostart entry does
     the same thing from cold.

=== If the keyboard dies ===

  * Switch VT:            Ctrl+Alt+F3
  * Log in there, then:   pkill -f ibus-ui-gtk3
                          pkill -x cosmic-voice
  * Back to the desktop:  Ctrl+Alt+F1  (or F2)

STEPS
