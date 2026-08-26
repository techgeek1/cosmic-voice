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
  `zwp_input_method_v2` preedit path exists behind `bind_input_method` but is
  **off by default**: a seat has one input-method slot, and binding it while
  IBus holds it wedges keyboard input session-wide on cosmic-comp (a smithay
  bug — see `docs/multiplexer.md`, which also designs the IM multiplexer that
  will make preedit and IBus coexist).
- **Vocabulary** biasing per application (hotwords boosted inside beam search,
  keyed by focused app_id) is scaffolded but not wired up yet; the config
  fields exist and default to empty.

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

`cosmic-voice enable|disable|start|stop|toggle|cancel` controls a running
instance from scripts or extra keybindings.

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
| `inject.rs`    | input-method-v2 and virtual-keyboard paths        |
| `toplevel.rs`  | focused app_id for prompt selection               |
| `engine.rs`    | state machine wiring the above together           |
| `ipc.rs`       | the applet/engine boundary                        |
| `transcript_log.rs` | raw transcript corpus, one JSON line per utterance |
| `app.rs`       | panel applet, icon and settings popup             |
