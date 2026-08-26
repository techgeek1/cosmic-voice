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
  drive live preedit; one offline pass of `parakeet-unified-en-0.6b` with
  hotword biasing produces the text that actually gets committed. Transducers
  also emit nothing during silence, which matters because push-to-talk brackets
  every utterance with silence and whisper hallucinates there.
- **Injection** is a synthesised keymap over `zwp_virtual_keyboard_v1`: it
  reaches every client, types arbitrary Unicode, and never contends with a real
  IME. By default nothing is typed until the offline result lands; set
  `fallback_partials: StreamOnly` to type the streaming model's stable prefix
  live, at the cost that a typed word cannot be taken back. The
  `zwp_input_method_v2` preedit path exists behind `bind_input_method` but is
  **off by default**: a seat has one input-method slot, and binding it while
  IBus holds it wedges keyboard input session-wide on cosmic-comp (a smithay
  bug — see `docs/multiplexer.md`, which also designs the IM multiplexer that
  will make preedit and IBus coexist).
- **Vocabulary** biasing per application (hotwords boosted inside beam search,
  keyed by focused app_id) is scaffolded but not wired up yet; the config
  fields exist and default to empty.

## Setup

Map a key to F13 on your keyboard (e.g. via VIA/Keychron Launcher), fetch both
models with `just models` (~1.3GB into `~/.local/share/cosmic-voice`), then
`sudo just install` and add **Voice** to the panel through COSMIC's applet
settings. No XKB changes are needed: evdev sits below the keymap, so it does
not matter that the default `us` layout maps keycode 183 to `XF86Tools`.

Settings live in `~/.config/cosmic-voice/config.ron`, written with commented
defaults on first run. `cosmic-voice enable|disable|start|stop|toggle|cancel`
controls a running instance from scripts or extra keybindings.

The panel spawns one applet process per output; the instances elect a single
primary that owns the microphone, the hotkey, and the resident models, and the
rest mirror it, so every panel icon works and nothing runs multiplied.

## Layout

| file           | role                                              |
| -------------- | ------------------------------------------------- |
| `hotkey.rs`    | evdev watcher, `EVIOCSMASK` filter, udev hotplug  |
| `audio.rs`     | continuous capture, pre-roll ring buffer          |
| `vad.rs`       | trailing-silence detection and release backstop   |
| `asr.rs`       | streaming + offline recognisers, hotword biasing  |
| `inject.rs`    | input-method-v2 and virtual-keyboard paths        |
| `toplevel.rs`  | focused app_id for prompt selection               |
| `engine.rs`    | state machine wiring the above together           |
| `ipc.rs`       | the applet/engine boundary                        |
| `app.rs`       | panel applet, icon and settings popup             |
