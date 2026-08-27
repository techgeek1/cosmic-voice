# cosmic-voice

Local push-to-talk dictation for the COSMIC desktop. Speech goes in through a
dedicated key, text comes out in whatever window has focus. Nothing leaves the
machine.

## How it works

- **Trigger** is read straight off evdev, because cosmic-comp's shortcut system
  only spawns processes on key *press* and `xdg-desktop-portal-cosmic` does not
  implement GlobalShortcuts, so neither can report a release. `EVIOCSMASK`
  restricts the descriptor to the trigger keycode and suppresses `MSC_SCAN`, so
  no other keystroke is ever delivered to this process. No root, no `input`
  group: logind's `uaccess` ACL is enough.
- **Capture** runs continuously into a ring buffer, so the ~750ms before the key
  registers is still there and the first syllable survives. It is a native
  PipeWire client so the callback rides the RT data-loop and keeps being
  serviced when the machine is saturated.
- **Recognition** uses two resident int8 transducers on the CPU, because
  provisional and final text want opposite things.
  `nemotron-3.5-asr-streaming-0.6b` decodes incrementally at 560ms chunks to
  drive live preedit; offline passes of `parakeet-unified-en-0.6b` with hotword
  biasing produce the text that actually gets committed. Transducers also emit
  nothing during silence, which matters because push-to-talk brackets every
  utterance with silence and whisper hallucinates there.
- **Long utterances are decoded as you speak them.** The offline pass costs
  more than linearly in audio length — measured here at four threads, 30s
  decodes at 0.052× real time but 120s at 0.093× — so waiting for the key to
  come up before starting it makes a minute of dictation land seconds late.
  Instead the engine cuts the recording at pauses of `segment_pause_ms` and
  decodes each finished segment while the next one is still being spoken, so
  releasing the key only ever decodes the tail. Measured on 60s of dictation:
  3.9s of waiting before, 0.35s after. Cuts land inside silence, so no word is
  split, and utterances too short to reach `min_segment_ms` of speech take the
  single-pass path exactly as before. The two recognisers run on threads of
  their own, so a segment decode never stalls the partials.
- **Injection** is a synthesised keymap over `zwp_virtual_keyboard_v1`: it
  reaches every client, types arbitrary Unicode, and never contends with a real
  IME. By default nothing is typed until the offline result lands; set
  `fallback_partials: StreamOnly` to type the streaming model's stable prefix
  live, at the cost that a typed word cannot be taken back. The
  `zwp_input_method_v2` preedit path — live text the application renders as
  pending, revised as often as the recogniser changes its mind — is what
  `input_method: Multiplexer` turns on, and it is **off by default**: a seat has
  one input-method slot, and binding it while IBus holds it wedges keyboard
  input session-wide on cosmic-comp (a smithay bug). Turning it on means taking
  the slot over from IBus rather than fighting it for it, which is a
  configuration change to the session as well as to this program — see
  "Multiplexer cutover" below.
- **Vocabulary** biasing per application (hotwords boosted inside beam search,
  keyed by focused app_id) is scaffolded but not wired up yet; the config
  fields exist and default to empty.

## Input-method multiplexer

`docs/multiplexer.md` designs it: one process owns the seat's
`zwp_input_method_v2` slot and multiplexes it between dictation and IBus, so
typed CJK and spoken text stop competing for a compositor slot that has no
sharing semantics. All five phases are in the tree. It is off unless
`input_method: Multiplexer` is set and the session's autostart has been cut
over, so the default install behaves exactly as it always did.

`src/ibus` is the upstream leg: a hand-written D-Bus client for ibus-daemon's
own private bus, with address discovery, the `IBusSerializable` codec for
preedit, candidates and engine descriptions, and the synchronous key path
where `ProcessKeyEvent` blocks and the effects it withheld are drained back
out of a property.

`src/im` is the downstream leg: the Wayland input method. It binds the slot on
a named display, takes a keyboard grab and a virtual keyboard for as long as a
text field has focus, resolves each key through the compositor's own keymap,
asks the engine about it, and then either lets the engine's commit stand,
types the character as text, or replays the key to the application untouched.
It runs one calloop loop over the Wayland connection, a channel carrying
IBus's asynchronous signals, and a timer that does key repeat and
reconnection.

`im/popup.rs`, `im/render.rs` and `im/theme.rs` are the candidate window that
`ibus-ui-gtk3` used to draw. A `wl_surface` given the `zwp_input_popup_surface_v2`
role is created once and kept for the life of the process — the compositor
re-parents it to each newly focused field and anchors it under the caret itself,
so there is nothing for a per-activation surface to buy — and candidates are
painted into `wl_shm` buffers with `tiny-skia`, shaped with `cosmic-text` so that
Japanese renders as Japanese, in colours read once from the COSMIC theme.
Hiding it is a null-buffer commit, because the protocol has no request for it.
Redraws coalesce twice over: once across the burst of signals a single keystroke
produces, and again on `wl_surface.frame`, so one keystroke is at most one
buffer. It is display-only — paging keys reach mozc as ordinary keys and there
is no pointer handling yet.

`im/switcher.rs` and `ibus/panel.rs` are the panel duties the retired
`ibus-ui-gtk3` leaves behind: without something registering the engine-switch
trigger with ibus-daemon, Super+Space just types a space. The accelerators come
from dconf (`org.freedesktop.ibus.general.hotkey triggers`) or from the config,
are registered as the daemon's global shortcut keys with their Shift-modified
backward twins, and a fired trigger cycles the engine over `preload-engines`.
It stays entirely inert while `ibus-ui-gtk3` is still the daemon's panel: the
registration is a last-writer-wins global with no unregister, and the daemon
broadcasts the response to every subscriber, so two panels would fight over
one hotkey.

`im/dictation.rs` is the turn-taking the whole thing exists for. When the
trigger goes down the engine sends a `Begin`: whatever conversion mozc had in
progress is *committed* into the field rather than discarded — the preedit is
held under `ClientCommitPreedit`, so finishing it is our choice to make — and
only then is the context reset. Streaming partials become the preedit,
revised in place; the offline result is committed as text and the keyboard
gets the turn back. Keys pressed mid-utterance keep going to IBus, because
they are rare and dropping them is worse. If the text field goes away halfway
through, the frontend says so, the engine finishes the utterance on the
virtual keyboard, and nothing is committed into whichever window took focus.

`engine.rs` never learns any of that. It hands text to a `TextSink` (`sink.rs`)
which either sends a command down a channel to the input-method thread or falls
back to the virtual keyboard, choosing between them by reading one flag that
thread publishes; neither side ever blocks on the other. `im/supervisor.rs`
keeps the thread alive — a frontend that dies restarts with a backoff, and the
applet popup says so meanwhile, because an input method that silently is not
there looks exactly like one that is.

The routing rules are a pure function in `im/router.rs` with unit tests, which
is the part that can be checked without a compositor; the turn-taking in
`im/dictation.rs`, and accelerator parsing and the cycle arithmetic in
`im/switcher.rs`, are the same. The rest is checked by
`scripts/im-harness/frontend-test.sh`, which starts a nested cosmic-comp, an
isolated ibus-daemon with mozc, a GTK entry and the frontend, injects keys over
libei, and asserts that `konnnitiha` comes out of the entry as こんにちは, that
plain keys arrive as committed text, that Ctrl-A still reaches the application
as a key, that a held key repeats at the right rate, and that killing
ibus-daemon leaves typing working. `scripts/im-harness/candidate-test.sh` does
the same for the candidate window: it converts with space and Tab and asserts
that the compositor sent a caret rectangle, that mozc's lookup table arrived,
that a plausibly sized buffer was attached, and that the window went away on
commit. `scripts/im-harness/switcher-test.sh` does
the same for the panel duties: it injects Ctrl+Alt+Space and asserts that the
engine toggles both ways, that the Shift-modified trigger comes back flagged
backward, and that mozc converts afterwards. None of it touches the live
session.

Three hidden devtests exercise the two legs. `cosmic-voice devtest ibus-info`
is read-only — it connects, decodes the engine registry and reports what the
daemon says — and is safe to run at any time. `cosmic-voice devtest ibus-keys
<engine> <key>…` feeds a scripted key sequence to a real engine and prints
what came back; it is **attended only**, because taking IBus focus takes it
away from whatever window you were actually typing in until it exits.
`cosmic-voice devtest im-frontend <wayland-display> [ibus-address]` runs the
frontend itself, and **refuses to run on the display it inherited**: binding
a second input method on a seat that already has one does not fail cleanly on
cosmic-comp, it wedges the keyboard session-wide, so the display is named
explicitly and the live one has to be asked for with `--live-i-know`. Point it
at a nested compositor. `--config <path>` gives it a config file other than the
user's, which is how the harness pins the engine-switch trigger and the engine
cycle to known values, and `--dictation-fifo <path>` reads newline-delimited
JSON dictation commands off a fifo, which is how a shell script drives
turn-taking with no microphone in the loop.

## Setup

Map a key to F13 on your keyboard (e.g. via VIA/Keychron Launcher) or rebind
the trigger from the popup once it is running, fetch both models with `just models` (~1.3GB into `~/.local/share/cosmic-voice`), then
`sudo just install` and add **Voice** to the panel through COSMIC's applet
settings. No XKB changes are needed: evdev sits below the keymap, so it does
not matter that the default `us` layout maps keycode 183 to `XF86Tools`.

Settings live in `~/.config/cosmic-voice/config.ron`, written with commented
defaults on first run. `asr_threads` (2) sizes the streaming recogniser and
`offline_threads` (4) the offline one, which is separate because nobody waits
on a partial but everybody waits on the commit; past four threads the offline
model stops getting faster. `segment_pause_ms` (500) is the trailing silence
that ends a segment inside a long utterance and `min_segment_ms` (3000) the
speech a segment must carry before a pause may end it — set `segment_pause_ms`
to 0 to disable segmentation and decode every utterance in one pass.

`input_method` is `Off` by default and the other choice is `Multiplexer`; it is
what decides whether this process owns the seat's `zwp_input_method_v2` slot.
`Off` is the shipped behaviour: nothing here binds the slot, IBus keeps its own
Wayland bridge, and dictated text is typed through the virtual keyboard.
`Multiplexer` makes the applet's primary process run the input-method frontend
against the live display, so typed CJK and spoken text go through the same slot
by turns — but only after the autostart cutover below, because two processes
cannot hold it. Set it without doing the cutover and the applet says
"Input method: blocked" in its popup and keeps working the old way. The retired
`bind_input_method` flag is accepted for one release, warned about, and ignored:
the mode is only ever what `input_method` says.

Two more belong to the input-method multiplexer and are empty by default, which
means "use the desktop's own settings". `ibus_triggers` is the engine-switch
accelerators in GTK syntax (`["<Control><Alt>space"]`); empty reads dconf
`org.freedesktop.ibus.general.hotkey triggers`, the key IBus's own settings
edit. `ibus_engines` is the engine ids the trigger cycles through
(`["xkb:us::eng", "mozc-jp"]`); empty reads dconf `preload-engines` in the order
`engines-order` remembers. Setting either overrides what the rest of the desktop
believes, and whatever ends up in `ibus_triggers` is registered with ibus-daemon
as a global shortcut that only a daemon restart clears — so they exist for
testing a known value, not as the normal way to configure this.

`cosmic-voice enable|disable|start|stop|toggle|cancel` controls a running
instance from scripts or extra keybindings.

## Multiplexer cutover

Turning `input_method: Multiplexer` on means taking the seat's input-method
slot away from `ibus-ui-gtk3 --enable-wayland-im`. There is no handover in the
protocol and no safe way to probe who holds it, so this is an **attended**
procedure: do it at the keyboard, with a second terminal or a spare VT open.
The persistent part is one line of one file, which is what makes the rollback
cheap.

    scripts/cutover.sh        # or: just cutover

It checks the preconditions, backs `~/.config/autostart/ibus-wayland.desktop`
up to `ibus-wayland.desktop.pre-multiplexer`, rewrites
`Exec=ibus start --type wayland` to `Exec=ibus start`, and then prints the
steps below and stops. It deliberately does not stop, start or reconfigure
anything that is running: the instant the slot changes hands is the one worth
watching.

Then, by hand, in this order:

1. Put `input_method: Multiplexer` in `~/.config/cosmic-voice/config.ron`.
2. Retire IBus's bridge in the session that is already running — the autostart
   line only takes effect at the next login:

       ibus exit
       ibus start
       pgrep -af -- --enable-wayland-im     # must print nothing

   ibus-daemon, mozc and the XIM server for XWayland clients all come back;
   only the Wayland IM bridge and the panel do not. Typed CJK stops working
   between these two commands and the next step.
3. Restart the applet, so it reads the new config and binds the slot: remove
   **Voice** from the panel and add it again through COSMIC's applet settings,
   or log out and back in.
4. Check the popup. `Ready — hold F13 to talk · Mozc あ` means the multiplexer
   is running and IBus has told it which engine is in effect;
   `Input method: blocked — …` means step 2 did not take (redo it, the applet
   notices within seconds and needs no restart); `Input method: not running —
   …` names the reason the frontend failed. Then type into a text field —
   mozc must convert, with cosmic-voice drawing the candidate window — and
   hold the trigger and talk, which should now arrive as preedit.
5. Log out and back in once, to confirm the same result from cold.

To go back:

    scripts/rollback.sh       # or: just rollback

which restores the backup and prints the reverse order: set
`input_method: Off` and restart the applet **first**, so the slot is free, then
`ibus exit; ibus start --type wayland`.

**If the keyboard dies.** This is the failure the whole design is built around
and it has happened once: two binders on one seat, under the smithay revision
cosmic-comp pins, leave a keyboard grab that nobody services, and no window
receives any key. It is survivable and the recovery does not need the desktop:

    Ctrl+Alt+F3                  # switch to a text VT and log in
    pkill -f ibus-ui-gtk3
    pkill -x cosmic-voice
    Ctrl+Alt+F1                  # (or F2) back to the desktop
    scripts/rollback.sh

Killing either holder is enough, because a grab is released by its object's
destructor and the object dies with the process. If the VT switch does not
work either, ssh in from another machine and run the same two `pkill`s.

The popup has two switches and a key. **Dictation** arms or disarms the
trigger. **Log transcripts** appends every committed utterance as a JSON line
to `~/.local/share/cosmic-voice/transcripts.jsonl`, raw from the recogniser,
as a corpus for evaluating a cleanup pass; logging starts off unless
`log_transcripts: true` is set in the config, and `cosmic-voice log on|off`
switches it from scripts. **Trigger key** shows the current binding; press it
and then the key you want (Esc cancels, ten seconds and it gives up), and the
choice takes effect immediately and is written back to the config. During
that window, and only then, the watcher reads every keyboard unmasked;
`cosmic-voice rebind` starts it from scripts.

The panel spawns one applet process per output; the instances elect a single
primary that owns the microphone, the hotkey, and the resident models, and the
rest mirror it, so every panel icon works and nothing runs multiplied.

## Layout

| file           | role                                              |
| -------------- | ------------------------------------------------- |
| `hotkey.rs`    | evdev watcher, `EVIOCSMASK` filter, udev hotplug  |
| `audio.rs`     | continuous capture, pre-roll ring buffer          |
| `vad.rs`       | trailing-silence detection, segment cut points    |
| `asr.rs`       | streaming + offline recognisers, hotword biasing  |
| `inject.rs`    | virtual-keyboard injection, and the retired IM path |
| `sink.rs`      | which of the two paths dictated text takes        |
| `toplevel.rs`  | focused app_id for prompt selection               |
| `engine.rs`    | state machine wiring the above together           |
| `ipc.rs`       | the applet/engine boundary                        |
| `transcript_log.rs` | raw transcript corpus, one JSON line per utterance |
| `app.rs`       | panel applet, icon and settings popup             |
| `ibus/`        | D-Bus client for ibus-daemon, the multiplexer's upstream leg |
| `im/`          | Wayland input-method frontend, the multiplexer's downstream leg |
| `im/dictation.rs` | turn-taking between the keyboard and the microphone |
| `im/supervisor.rs` | keeping the input-method thread alive inside the applet |
| `im/switcher.rs` + `ibus/panel.rs` | the IBus panel duties: engine-switch trigger and engine cycle |
| `im/popup.rs`  | the candidate window's surface, shm buffers and lookup-table state |
| `im/render.rs` | painting candidates, with tiny-skia and cosmic-text        |
| `im/theme.rs`  | the candidate window's colours, from the COSMIC theme      |
