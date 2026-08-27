# The input-method multiplexer

Design for cosmic-voice v2: one process owns the seat's `zwp_input_method_v2`
slot and multiplexes it between voice dictation and IBus (mozc). Typed CJK and
spoken text stop competing for a compositor slot that has no sharing semantics;
turn-taking becomes an internal routing decision, which matches reality — one
human, one input stream at a time.

Everything below is grounded in source analysis of the four components we sit
between, done 2026-08-25: ibus 1.5.34 (the installed version; bridge code in
`client/wayland/ibuswaylandim.c`), mozc 3.34, cosmic-comp `31827ed` with its
pinned smithay `5fb12b8`, and the live session's D-Bus surface. Citations are
into those trees.

## Why the slot cannot be shared (and why probing it broke the keyboard)

`zwp_input_method_v2` is connection-scoped: one IM per seat, freed only when
the holder destroys its object. No preemption, no handoff. On cosmic-comp it
is worse than exclusive — smithay's `add_instance`
(`wayland/input_method/input_method_handle.rs:58-69`) has a variable-shadowing
bug: when a second client binds, `unavailable` is sent to the **existing**
instance instead of the new one. That is the 2026-08 incident mechanism: our
probe told *IBus* it was dead; IBus destroyed its IM object but not its
`zwp_input_method_keyboard_grab_v2`, and smithay releases the seat-wide grab
only in the grab object's destructor (`input_method_keyboard_grab.rs:102-105`).
Every key kept routing to a grab nobody was servicing — session-wide dead
keyboard until IBus was killed.

Consequences, non-negotiable:

- **We must be the sole binder by construction.** There is no safe probe, and
  no `unavailable` will ever arrive on our object under current smithay.
  Startup safety check: refuse to bind if an `ibus-ui-gtk3` process with
  `--enable-wayland-im` is running.
- The autostart changes from `ibus start --type wayland` to
  `ibus-daemon --xim --panel disable` — not a bare `ibus start`, which on a
  Wayland session launches the bridge too (phase-5 finding 10). ibus-daemon,
  mozc, and `--xim` (XWayland clients) all stay.
- The smithay bug should be filed upstream (one rename plus object-identity
  checks in `add_instance` / `destroyed` / `GrabKeyboard`, which currently
  also let a superseded binder steal the grab).

## What we are actually replacing

`ibus-ui-gtk3 --enable-wayland-im` plays **two roles**, and we inherit both:

1. **The Wayland IM bridge** (`client/wayland/ibuswaylandim.c`): binds the IM
   slot, grabs the keyboard per activation, forwards keys to ibus-daemon,
   relays commit/preedit back.
2. **The panel** (`org.freedesktop.IBus.Panel`): despite `--panel disable`
   (which only stops the *daemon* spawning one, `bus/main.c:80`), the UI
   process registers as the panel unconditionally (`ui/gtk3/application.vala:111`)
   and it is what renders the mozc candidate window today, on a surface handed
   to the compositor via `zwp_input_method_v2_get_input_popup_surface`
   (`ibuswaylandim.c:2884-2894`). It also registers the engine-switch hotkey
   with the daemon (`panel.vala:529-563`); without a panel doing that,
   **Super+Space just types a space**.

There is no partial replacement: mozc's candidate routing is decided at engine
startup from `~/.config/mozc/ibus_config.textproto`'s
`compatible_wayland_desktop_names` allowlist (`["GNOME"]`;
`src/unix/ibus/mozc_engine.cc:652-681`), COSMIC is not on it, so candidates
always go down the IBus lookup-table path. `mozc_renderer` is a dead end
regardless: Qt6 forced to XCB (`src/renderer/qt/qt_server.cc:95-98`),
positioned by absolute X11 coordinates that a Wayland IM client never has.
If our client sets `IBUS_CAP_LOOKUP_TABLE`, the daemon delivers
`UpdateLookupTable` signals directly to our input context
(`bus/inputcontext.c:2334-2341`) — so we render candidates, in an IM popup
surface that cosmic-comp positions at the caret for us (verified implemented:
`src/wayland/handlers/xdg_shell/popup.rs:178-205`, with edge clamping and
vertical flip).

## Architecture

```
                       cosmic-voice process
  ┌───────────────────────────────────────────────────────────┐
  │  engine (existing)          im thread (new)               │
  │  ┌──────────────┐  chans   ┌─────────────────────────┐    │
  │  │ hotkey/audio │◄────────►│ router state machine    │    │
  │  │ asr/vad      │          │  ┌───────────┐          │    │
  │  └──────────────┘          │  │ wayland   │ IM slot, │    │
  │                            │  │ frontend  │ grab, vkbd,   │
  │                            │  │           │ popup surface │
  │                            │  ├───────────┤          │    │
  │                            │  │ ibus      │ zbus,    │    │
  │                            │  │ client    │ private bus   │
  │                            │  ├───────────┤          │    │
  │                            │  │ candidate │ shm +    │    │
  │                            │  │ renderer  │ tiny-skia│    │
  │                            │  └───────────┘          │    │
  │                            └─────────────────────────┘    │
  └───────────────────────────────────────────────────────────┘
```

The IM stack lives on its own thread with its own event loop (calloop over the
Wayland fd + zbus fd + channel fds). Rationale: key forwarding is
latency-critical and must never wait behind ASR events; the Wayland IM
connection, the vkbd, and the popup surface must share one `wl_display`; and
the sync ProcessKeyEvent call (below) blocks, which must not stall the engine.
The existing `inject.rs` VK path stays as the fallback injector for
non-text-input clients and remains usable when the multiplexer is disabled.

### The ibus client (upstream leg)

Hand-written zbus proxies — the crate ecosystem has nothing maintained on the
client side (nearest: `ibus-rs`, archived 2022; `librush` is engine-side but a
useful IBusText reference).

- **Connection**: address file at
  `~/.config/ibus/bus/<machine-id>-unix-<WAYLAND_DISPLAY>` (shell-style
  `IBUS_ADDRESS=` / `IBUS_DAEMON_PID=` lines; validate PID liveness;
  `IBUS_ADDRESS` env overrides). It is a **full message bus** run by
  ibus-daemon itself — SASL EXTERNAL, standard `Hello`, daemon is `:1.0`.
  Signals to us are unicast. Watch `org.freedesktop.IBus` on the session bus
  for daemon restarts; reconnect and rebuild contexts.
- **Context lifecycle**: `CreateInputContext("wayland-cosmic-voice")` —
  the name is load-bearing, see finding 1 below →
  `Properties.Set ClientCommitPreedit=(true)` →
  `EffectivePostProcessKeyEvent=(true)` → `SetCapabilities(PREEDIT_TEXT |
  FOCUS | SURROUNDING_TEXT | LOOKUP_TABLE | AUXILIARY_TEXT)` → `FocusIn` on
  activation. One context reused across activations (FocusIn/FocusOut), like
  GTK; destroy via `org.freedesktop.IBus.Service.Destroy`.
  Note the capability set *differs* from the bridge's: adding LOOKUP_TABLE +
  AUXILIARY_TEXT routes candidates to us instead of the (now absent) panel.
- **Key events**: `ProcessKeyEvent(keyval, keycode, state) -> handled`.
  keyval = X keysym resolved through our xkb state; keycode = **evdev+8**
  (the GTK convention every engine is tested against — the ecosystem is
  inconsistent and the daemon passes it through untouched); state = IBus
  modifier bits with `RELEASE_MASK` (1<<30) on key-up.
- **Sync mode**: call ProcessKeyEvent blocking, then drain
  `Properties.Get PostProcessKeyEvent` (`a(yv)` of commit / forward /
  preedit / delete-surrounding records the daemon withheld during
  processing). This is GTK's default since 1.5.28 and eliminates the classic
  async commit-vs-passthrough race by construction. The drain records encode
  non-text payloads printf-style inside an IBusText (e.g. `"keyval,keycode,state"`
  for forward-key) — ugly, small, fixed set.
- **IBusText codec**: every `v text` is `("IBusText", a{sv}, s, v)` with the
  attr list `("IBusAttrList", a{sv}, av)` of
  `("IBusAttribute", a{sv}, u type, u value, u start, u end)`, offsets in
  unicode chars. Underline/background attributes drive preedit styling.

### The Wayland frontend (downstream leg)

Follows the bridge's proven shape, minus its bugs:

- Bind `zwp_input_method_v2` once (post safety check). `activate`/`deactivate`
  are double-buffered — apply on `done`; `im_serial` increments once per
  `done`; every text operation is its own `commit(serial)`.
- **Per-activation** virtual keyboard + keyboard grab, both destroyed on
  deactivate. This matches the bridge (Sway keymap workaround), matches
  smithay's lifecycle, bounds our crash blast-radius (grab-object death always
  restores key flow — verified cleanup path), and limits exposure to
  cosmic-comp's silently-stealable grab (xdg popup grabs replace the IM grab
  with no signal; re-grabbing per activation wins it back).
- Grab `keymap` fd: forward verbatim to the vkbd (gate `key`/`modifiers` on
  having sent one; never forward size==0); parse into our xkb state for keysym
  resolution. Unlike the bridge, re-parse on *every* keymap event. Grab
  `modifiers`: update xkb state, map to IBus bits, forward raw quadruplet to
  the vkbd.
- **Key routing** (the bridge's `post_key` contract, which we must replicate):
  1. Key from grab → resolve keysym → sync ProcessKeyEvent + drain.
  2. Handled → done (effects arrived via the drain).
  3. Unhandled press of a plain printable char (no non-Shift mods) →
     **commit it as text** (`commit_string`), not vkbd. This is how
     engine-declared layouts work (engine says `ru`, compositor keymap `us`);
     apps see committed text for plain typing while an IM context is active.
  4. Everything else unhandled → replay through the vkbd with original
     timestamp and evdev code. Safe: smithay delivers vkbd keys directly to
     the focused surface's `wl_keyboard`s, bypassing both the shortcut filter
     and the grab — no feedback loop, no shortcut re-triggering. (Watch item:
     cosmic-comp has staged an unused `InputBackendId::VirtualKeyboard`; if a
     future version routes vkbd keys through the input filter, we need loop
     suppression. Never inject via libei while holding the grab — that path
     *does* loop.)
  5. Key repeat is ours to implement (compositor sends the grab `repeat_info`
     once, at grab time, and never synthesizes repeats): re-inject presses at
     delay/rate, cancel on any release, any new press, or context change. Fix
     the bridge's bug of using `rate` (chars/sec) as a millisecond period.
- **Signal → protocol mapping**: `CommitText` → `commit_string` + `commit`;
  `UpdatePreeditTextWithMode` → `set_preedit_string` + `commit` (and because
  `ClientCommitPreedit=true`, if the last mode was COMMIT we must commit the
  preedit text ourselves on deactivate/reset — the bridge drops it, a real
  mozc data-loss bug we fix for free); `DeleteSurroundingText` → byte-converted
  `delete_surrounding_text` + `commit`; `ForwardKeyEvent` → vkbd (the bridge
  left this as a TODO on v2); `RequireSurroundingText` → `SetSurroundingText`
  from the cached `surrounding_text` events. Relay `content_type` → 
  `ContentType` property (hint/purpose tables in `ibuswaylandim.c:2230-2323`).
- Compositor shortcuts (Alt-Tab, Super bindings, VT switch, modifier-only
  binds) are filtered before grab dispatch in cosmic-comp — they never reach
  us, by construction. Non-text-input clients never activate the IM, so we
  hold no grab over them and typing there is untouched native flow.

### Candidate renderer

A `wl_surface` on the IM connection given the
`zwp_input_popup_surface_v2` role; cosmic-comp anchors it below the caret
rectangle and handles flip/clamping. We draw the `IBusLookupTable`
(candidates, labels, focused index, page, orientation) + auxiliary text into
shm buffers — `tiny-skia` for painting, `cosmic-text` for shaping (CJK is the
whole point), theme colors sampled from the COSMIC theme config. Paging keys
reach mozc as ordinary keys, so the UI is display-only at first;
`CandidateClicked` can come later with pointer support on the popup.

An IM popup exists only while a text-input parent is focused — exactly when
candidates exist, so no gap.

### Panel duties (minimum viable)

Register the configured triggers via
`IBus.Bus.set_global_shortcut_keys(IME_SWITCHER, ...)` (read them from
`org.freedesktop.ibus.general.hotkey triggers` dconf). The daemon then
consumes the trigger inside ProcessKeyEvent (returns handled) and emits
`GlobalShortcutKeyResponded`; we respond by cycling: `SetGlobalEngine(next)`
over the dconf `preload-engines` list (`ActiveEngines` is empty in practice,
finding 2 below). No switcher popup in v1 — press
cycles, the panel applet can display the current engine name. (The daemon's
own release-signal latency workaround suggests keeping any future
press/release switcher state client-side.)

Four corrections from building it, all detailed in the phase-4 findings: the
response signal is a **broadcast filtered by `AddMatch`**, so subscribing is
not optional and two subscribers both act; the `keycode` field of a
registration is the **backward flag**, not a keycode; the modifiers must be
spelled in the bits that survive `IBUS_MODIFIER_FILTER`, which means `MOD4`
rather than `SUPER`; and because the response is broadcast and the property is
last-writer-wins with no unregister, the panel role is exclusive in practice —
the switcher stays entirely inert while `ibus-ui-gtk3` still holds it.

## Turn-taking (the point of all this)

Router states, one per activation — wrong on the scope, see phase-5 finding 1:
the turn is process state that a deactivation *ends*, because an utterance
outlives an activation and the commit has to fall back to the virtual keyboard
rather than land in whatever window took focus.

- **Forwarding** (default while a text field is focused): keys → ibus as
  above. Dictation idle.
- **Dictating** (F13, from the evdev watcher as today): on entry, if mozc has
  visible preedit, commit it (we hold it under `ClientCommitPreedit`) — the
  user's half-typed conversion is finished, not discarded — then `Reset` the
  context and stop forwarding key *text* handling to preedit. Streaming
  partials drive `set_preedit_string`; release → offline result →
  `commit_string`; resume Forwarding. Keys pressed mid-dictation keep
  forwarding to ibus (rare, harmless, and dropping them is worse).
- **Unfocused**: no activation, no grab, no ibus focus. Dictation falls back
  to the existing VK/StreamOnly path (non-ti-v3 apps: Firefox/VS Code today).

The dictation preedit itself needs nothing new — `engine.rs` already speaks
preedit; it just becomes reachable because we finally own a bound IM.

## Failure and rollback story

- Our crash: grab + IM objects die with the connection → compositor restores
  normal key flow and sends the focused text-input `leave`. Rebind on restart
  immediately re-`enter`s the focused field. Run under a supervisor
  (systemd user unit) for auto-restart.
- ibus-daemon crash/restart: detected via session-bus name watch; contexts
  rebuilt; meanwhile keys pass through raw (English still types).
- Config flag `multiplexer: false` (or the existing `bind_input_method`
  evolving into a three-way mode) → today's shipped Option B behavior.
  Re-enabling IBus's own bridge is one autostart-line revert away. Shipped as
  `input_method: Off | Multiplexer`; the third state is deliberately not
  reachable, see phase-5 finding 4. The autostart revert is
  `scripts/rollback.sh`, and it is one line, as promised.

## Phases

1. **ibus client module** — proxies, IBusText codec, address discovery,
   sync+drain key path. Devtest: create a context, `SetEngine("mozc-jp")`,
   feed a scripted key sequence, print signal trace. (Deliberate, isolated
   run: FocusIn on a test context steals engine focus from real windows.)
2. **Frontend rework** — activation lifecycle, per-activation grab+vkbd, xkb
   state, key routing with commit-fallback and repeat. Milestone: with IBus's
   wayland-im disabled, typing (incl. mozc preedit/commit in a terminal) is
   indistinguishable from today — candidates not yet visible.
3. **Candidate popup** — shm renderer, lookup-table/aux signals, paging.
   Milestone: full mozc parity.
4. **Panel duties** — trigger registration + engine cycling.
5. **Dictation integration** — router turn-taking, config/autostart
   migration, applet status surface. Done; see the phase-5 findings.

Each phase is independently testable and abortable; through phase 3 the
rollback is trivial because nothing outside cosmic-voice changed permanently.

## Open decision points

1. **Event loop for the im thread**: calloop (idiomatic for wayland-client,
   used by every smithay-side client) vs polling inside the engine's tokio
   select. Recommendation: calloop on a dedicated thread. Phase 1 showed
   that zbus's blocking API does *not* hand over a pollable fd for free
   (finding 3 below): the choice for phase 2 is between ticking zbus's own
   executor from a calloop source and building zbus with `async-io` so its
   reactor fd can be registered directly.
2. **Candidate paint stack**: ~~tiny-skia + cosmic-text vs embedding iced~~.
   Closed by phase 3 as recommended: tiny-skia 0.11 and cosmic-text 0.19, both
   already in the graph through libcosmic's iced, so the whole renderer added
   nothing new to build. iced was never a real option — see phase-3 finding 1
   for what an input-popup surface actually is.
3. **Engine switcher UX**: cycle-only v1 (recommended) vs popup list.
4. **Upstream fix**: file the smithay `add_instance` bug with the incident
   as the repro narrative. Costs an afternoon, benefits everyone who ever
   binds this protocol on COSMIC. Recommended.

## Build strategy (banked 2026-08-25)

Status: **phases 1 to 5 done, cutover done (2026-08-27, findings 10 and 11), phase 6 in progress** — `src/ibus/`,
`src/im/`, `src/sink.rs` and the `input_method` config mode. See the findings
sections below for what implementing them corrected in this document. All five
are code-complete, unit-tested and green in the nested harness, and none has
been run against a live seat: phase 4's switcher is deliberately inert on the
live daemon while `ibus-ui-gtk3` is still its panel (phase-4 finding 8), and
phase 5 ships the cutover as `scripts/cutover.sh` plus a README procedure
rather than as anything automatic. The cutover remains the one attended step,
together with the visual pass on the candidate window that no screenshot client
on this machine can automate (phase-3 finding 9).

Implementation will be agent-driven, which changes the bottleneck: the
mechanical bulk (proxies, codec, relay tables) is hours of wall clock, and the
gating resource is live-session verification time at the keyboard. Two
consequences:

- **Test harness first.** cosmic-comp runs nested under winit, and
  libei-injected keys traverse the full input filter → IM grab dispatch path
  (unlike vkbd keys, which bypass it — smithay `virtual_keyboard_handle.rs`
  vs cosmic-comp `input/mod.rs:1998-2045`). So a nested cosmic-comp + libei
  key injection + a test text-input-v3 client + a scratch ibus-daemon/mozc
  (isolated via IBUS_ADDRESS/XDG overrides) exercises the whole
  grab → ProcessKeyEvent → commit/passthrough loop without touching the live
  seat. Candidate rendering verifies by screenshot in the nested session.
  This harness is what lets agents iterate on the routing state machine at
  machine speed; build it as part of phase 2.
- **Live-session work stays attended.** Anything binding the real seat's IM
  slot happens with the user present and a recovery shell open (incident
  history: see the slot-exclusivity section). Expected attended sessions:
  phase-1 devtest against the live daemon, the phase-2 cutover, a phase-3
  visual pass, final acceptance — roughly an hour each; total wall clock on
  the order of a week, with a tail of small divergence fixes surfaced by
  daily driving.

Prerequisite chores when resuming: file the smithay `add_instance` bug
upstream; decide the engine-switcher scope (default: cycle-only, no popup).

## Findings from phase 1 (2026-08-26)

Implementing the ibus client against ibus 1.5.34 source and the live daemon
turned up these corrections. Citations are into the 1.5.34 tree.

1. **The client name must start with `wayland`.** `bus/inputcontext.c:389-398`
   (`IGNORE_FOCUS_OUT_CONDITION`) compares the first seven characters of the
   name passed to `CreateInputContext` against `"wayland"`; for any other
   client, in what the daemon considers a Wayland session, it latches
   `ignore_focus_out` whenever a preedit becomes visible, after which
   `FocusOut` and `Reset` return without doing anything (`:1307-1311`,
   `:1332-1338`). That would break the turn-taking design, which resets the
   context on entering dictation. We use `"wayland-cosmic-voice"`. The
   "Wayland session" predicate is "some panel has called
   `SetGlobalShortcutKeys`" (`bus/ibusimpl.c:2667-2670`), so the trap arms
   itself the moment phase 4 registers the trigger.
2. **`ActiveEngines` is empty** (977 registry engines, 0 active) even with
   `xkb:us::eng` and `mozc-jp` configured; the daemon only fills it from
   loaded components. Engine cycling reads dconf
   `org.freedesktop.ibus.general preload-engines` (`['xkb:us::eng',
   'mozc-jp']` here) and resolves through `GetEnginesByNames`.
3. **zbus's blocking API owns a runtime.** Polling a `MessageStream` ticks the
   connection's internal executor, which holds the tokio socket; polling it
   from any other runtime panics ("there is no reactor running"). Phase 1
   routes signal waits through `zbus::block_on` with the timer constructed
   inside the future. Phase 2 cannot simply register the socket with calloop;
   see open decision 1.
4. `PostProcessKeyEvent` introspects as `(a(yv))` but the getter returns a
   bare `a(yv)` (`bus/inputcontext.c:260` vs `:1639-1648`). The decoder
   accepts both. The record encoding is: tag byte + `IBusText`, with
   non-text payloads printf'd into the text — `'c'` commit, `'d'`
   delete-surrounding `"%d,%u"` (signed offset), `'f'` forward-key
   `"%u,%u,%u"`, `'h'`/`'s'` hide/show preedit, `'r'` require-surrounding,
   `'u'` update-preedit as two records (text, then `"%u,%u"` cursor,visible),
   `'m'` update-preedit-with-mode as two records (text, then `"%u,%u,%u"`).
   `'m'` replaces `'u'` exactly when `ClientCommitPreedit` is set
   (`:3505-3507`); the queue caps at 30 (`MAX_SYNC_DATA`, `:34`).
5. **`DeleteSurroundingText` and `RequireSurroundingText` are emitted but not
   in the introspection XML** (`:2666-2686`, `:2693-2709`). Anything generated
   from introspection misses both; the phase-2 mapping depends on both.
6. **`GlobalShortcutKeys` and `PreloadEngines` are write-only** despite
   introspecting rw (`bus/ibusimpl.c:2142-2147`). Phase 4 cannot read back
   what it registered; keep the triggers client-side.
7. **Preedit delivery has a precondition**: `PREEDIT_CONDITION`
   (`bus/inputcontext.c:378-383`) sends preedit to the client only if
   `CAP_PREEDIT_TEXT` and (`EmbedPreeditText` or `CAP_FOCUS` unset).
   `EmbedPreeditText` is a daemon-global (true here) that any client can flip;
   with it off and our capabilities set, preedit goes to a panel that no
   longer exists. `devtest ibus-info` prints it; the frontend should assert it.
8. Minor: `IBUS_IGNORED_MASK` aliases `IBUS_FORWARD_MASK` (bit 25,
   `ibustypes.h:91`); the daemon drives sync mode from the
   `EffectivePostProcessKeyEvent` property, not `CAP_SYNC_PROCESS_KEY`;
   `mozc-jp` declares layout `default`, so the frontend's routing rule 3
   (commit plain printables as text) is what actually runs for it; zbus 5.19
   ships `connection::Builder::ibus()` but it shells out to `ibus address`
   and skips PID validation, so we discover the address ourselves.
9. **Per-context `SetEngine` is refused** while dconf `use-global-engine`
   is true (the default): "Cannot set engines when use-global-engine is
   enabled". `SetGlobalEngine` is the only switch, it moves every window at
   once, and the engine attaches to a context only on `FocusIn` (before that
   `GetEngine` answers the placeholder `dummy`). Engine switching in phase 4
   is therefore global by construction, which matches the panel's semantics.
10. **`mozc-jp` starts in direct mode** (`active_on_launch: False` in the
   user's `ibus_config.textproto`) and passes every key through unhandled;
   `mozc-on` starts in hiragana. Live run 2026-08-26 through `ibus-keys`:
   every press `handled=true`, preedit arrives in the drain as `'m'` records
   with `mode=commit`, lookup tables and auxiliary text arrive as ordinary
   signals, `CommitText` lands in the drain on Return. The sync contract holds.

## Findings from phase 2 (2026-08-26)

Building the Wayland frontend against cosmic-comp's protocols, ibus 1.5.34's
bridge and xkbcommon turned up these corrections. Citations are into ibus
1.5.34 and the protocol XML shipped in `wayland-protocols-misc 0.3.12`.

1. **The event-loop problem dissolves; open decision 1 is closed by neither of
   its options.** Building zbus with `default-features = false, features =
   ["blocking-api", "async-io"]` puts the connection's executor on a thread
   zbus owns, so the blocking proxies park only the calling thread and work
   from anywhere. No fd is registered with calloop and nothing ticks zbus's
   executor; the calloop thread simply calls `Context::process_key` inline and
   blocks for the engine round-trip, which is the contract the sync key path
   wanted in the first place. This supersedes finding 3 from phase 1 — the
   `tokio::time::timeout`-inside-`block_on` construction is gone with it.
2. **One context needs two threads.** `process_key` is called inline from the
   loop, and *something* must stay blocked in the signal stream because zbus's
   per-stream queue is bounded (64) and a full queue stalls the whole
   connection. One `&mut Context` cannot be in both places, so
   `Context::take_signals` detaches the stream and it moves to a thread whose
   only job is to forward into a `calloop::channel`. The stream ending is also
   the daemon-death detector, which is more reliable than watching for a
   transport error on a call nobody happened to make.
3. **The key routing list needs a rule 0: with no IBus context, replay
   everything.** Rule 3 as written commits plain printable presses as text,
   which while ibus-daemon is restarting turns every keystroke into committed
   text — nearly right, and wrong exactly where it matters, because an
   application that acts on key events sees none. "Keys pass through raw" in
   the failure story has to be a routing rule, not an aspiration.
4. **`IBUS_MODIFIER_MASK & ~IBUS_SHIFT_MASK` is the wrong test for "plain".**
   It includes Caps Lock and Num Lock, so either one being on silently
   disables commit-as-text for as long as it stays on. Upstream already
   defines the right set: `IBUS_MODIFIER_FILTER` (`ibustypes.h:387-398`)
   excludes both locks, the mouse buttons, and the Super/Hyper/Meta aliases.
   The test is `MODIFIER_FILTER & ~SHIFT_MASK`. IBus's own bridge avoids the
   bug only by accident, by never reporting a lock at all (next finding).
5. **Modifiers come from `modifiers` events alone.** The bridge drives the xkb
   state from both `xkb_state_update_mask` and `xkb_state_update_key`
   (`ibuswaylandim.c:1240` against `:1708-1745`), which xkbcommon documents as
   alternatives — running both double-counts a latch. We use the mask path
   only, as every ordinary Wayland client does, and serialise the *locked*
   component as well as depressed and latched, so Caps Lock reaches the engine
   as `IBUS_LOCK_MASK` the way it does from GTK. The bridge serialises
   depressed|latched and cannot.
6. **Which keys repeat is a keymap question, not a policy one.**
   "Re-inject presses at delay/rate" repeats held modifiers too.
   `xkb_keymap_key_repeats()` answers it per keycode and nothing else does.
   The rate bug the doc identified is real and confirmed: `rate` is characters
   per second, so the period is `1000/rate` ms.
7. **text-input-v3 and IBus renumber `terminal`.** Purpose 13 against 10,
   which shifts date, time and datetime by one; and the hint bits share no
   positions at all. The doc pointed at the bridge's table without saying that
   a cast is actively wrong rather than merely unidiomatic.
8. **`delete_surrounding_text` cannot express every IBus request, and the
   bridge's version of it has never run.** IBus names a character range
   relative to the caret which need not contain it; text-input-v3 names a byte
   count before the caret and one after, so its range always does. We extend
   the range to reach the caret, over-deleting rather than under-deleting, on
   the grounds that an engine and an application disagreeing about the text is
   the worse failure. Upstream clamps with `offset = MIN (offset, 0)`
   (`ibuswaylandim.c:752`), which underflows the unsigned `after_length` for a
   forward-only range — but the entire surrounding-text path there, including
   `SetSurroundingText` and `RequireSurroundingText`, sits inside
   `#if ENABLE_SURROUNDING`, which is not defined. **There is no working
   reference implementation of surrounding text on this protocol.** Ours is
   the first, so it is the part of phase 2 most likely to diverge in practice.
9. **A `commit(serial)` with no preedit beside it clears the preedit.** Pending
   state that is unset is empty, so "every text operation is its own commit"
   has a consequence: a commit-only operation empties the preedit as a side
   effect. That is what engines want — they emit commit-then-preedit — but an
   engine emitting them in the other order would lose the preedit, and the
   rule reads as neutral in the design.
10. **The "data-loss bug we fix for free" cannot be fixed on focus change, and
    trying leaks the text into the next window.** The design's claim was that
    holding the preedit under `ClientCommitPreedit` lets us commit a
    half-finished conversion that IBus's bridge discards. Measured in the
    harness: with 「こ」 pending in window A, moving focus to window B put
    「こ」 **into B**. The mechanism is smithay's handler — `CommitString` is
    `with_active_text_input(|ti, _| ti.commit_string(…))`
    (`input_method_handle.rs:207-211`), so the text goes to whichever text
    input is active when the *compositor* processes the request, not the one
    that was focused when we sent it. By the time `deactivate` reaches us the
    old field is gone and the new one may already be in.
    text-input-v3 has no way to express "commit into the field that is
    leaving", so on a focus change the preedit is lost exactly as it is with
    the bridge. The flush is now guarded on still being active: it is a no-op
    on `deactivate` and correct on the `Reset` path, which is the one phase 5
    actually needs. Losing a preedit is bad; typing it into a different
    application is worse.
11. **A forwarded key with keycode 0 cannot be replayed.** A virtual keyboard
    sends keycodes, not keysyms, so an engine forwarding a bare keysym would
    need a keymap synthesised for it — the trick `inject.rs` already does for
    dictation. Logged and dropped for now, which is where the bridge left it
    too (`ibus_wayland_im_keysym` is `g_warning ("TODO")` on v2).
12. **The startup safety check in the doc is necessary but nowhere near
    sufficient.** "Refuse to bind if `ibus-ui-gtk3 --enable-wayland-im` is
    running" says nothing about the case where it is *not* running and the
    display is still the live seat the user is typing on. `im::run` takes the
    display name as an argument and refuses the one the process inherited from
    `$WAYLAND_DISPLAY` unless explicitly overridden, so the dangerous case has
    to be asked for rather than reached. The bridge check stays, hard-failing
    on the live display and warning on a nested one, where the other seat
    makes it harmless.

13. **The smithay `add_instance` behaviour reproduced, and it is survivable
    when *we* are the victim.** Two frontends were accidentally run against the
    nested compositor at once. The second one bound successfully and the first
    received `unavailable` — the new binder wins, the existing holder is
    evicted, exactly as `input_method_handle.rs:58-69` predicts. Our handler
    logs and stops the loop, and stopping destroys the grab object, so key flow
    was restored immediately. That is the difference between us and IBus's
    bridge in the 2026-08 incident: the bridge destroys its input method and
    keeps its grab, and it is the orphaned grab, not the eviction, that kills
    the session. Worth carrying into the upstream bug report as the contrast
    case.

Confirmed rather than corrected: `zwp_input_method_v2` on the live session is
held by `ibus-ui-gtk3 --enable-wayland-im` right now (pid 1985886), exactly as
the doc's first section describes, so the phase-2 cutover is an attended
operation that starts by changing the autostart line.

### What was verified, and how

`scripts/im-harness/` builds the whole loop with no live-session component:
a nested cosmic-comp under winit, an isolated ibus-daemon and mozc on a private
D-Bus session bus, a GTK3 `Gtk.Entry` speaking text-input-v3, and key injection
over libei. The last of those is the piece the design was unsure about, and it
works: cosmic-comp never listens on an EIS socket, but it hands one out over
`com.system76.CosmicComp.Ei.GetSenderSocket` on its own bus, and keys injected
that way traverse the shortcut filter and the input-method grab — the full
path, unlike virtual-keyboard keys, which bypass both.

`scripts/im-harness/frontend-test.sh` is the one-command regression, all seven
assertions green on 2026-08-27:

- mozc converts `konnnitiha` to こんにちは and Return commits it into the entry;
- with `xkb:us::eng`, unhandled plain keys arrive as committed text (rule 3);
- Ctrl-A reaches the application as a key (it selects all, and the next
  character replaces the field);
- a key held for 1.5s repeats 23 times — one press plus twenty-two at 40ms,
  measured 601ms delay and 41ms period, against the ~36 the bridge's
  rate-as-period bug would give;
- killing ibus-daemon leaves the frontend running and keys reaching the
  application as raw key events.

What still needs a human: anything on the live seat (the cutover itself), and
the visual pass — the nested compositor renders on the live desktop but there
is no scripted screenshot, so preedit styling and, from phase 3, the candidate
window are looked at rather than asserted.

## Findings from phase 4 (2026-08-27)

Registering the engine-switch trigger and cycling engines against ibus 1.5.34.
Citations are into the 1.5.34 tree unless stated.

1. **`GlobalShortcutKeyResponded` is a broadcast filtered by match rule, and
   nothing else.** The design and the phase-1 proxy left "who receives it" open.
   The answer: `bus_ibus_impl_emit_signal` builds a signal with **no
   destination** and hands it to `bus_dbus_impl_dispatch_message_by_rule`
   (`bus/ibusimpl.c:2477-2492`), whose recipient list is built by walking
   `dbus->rules` and asking each for its matching connections
   (`bus/dbusimpl.c:1996-2019`). The rules are the ones clients registered with
   `AddMatch` (`bus/dbusimpl.c:960-999`). So it goes to **every connection
   holding a matching match rule** — not to the client that called
   `SetGlobalShortcutKeys`, not to the focused input context's connection, and
   not to anybody who merely declared the signal in a proxy. A client that does
   not `AddMatch` sees nothing; two clients that do both see it and both act.
   That last part is why phase 4 refuses to run at all when `ibus-ui-gtk3` is
   the daemon's panel (below), rather than merely declining to register.
   ibus-daemon rewrites `sender='org.freedesktop.IBus'` in a rule to its own
   unique name as a special case (`bus/dbusimpl.c:976-978`), so the obvious
   rule text works.
2. **The `keycode` field of a registration is the backward flag, not a
   keycode.** `is_backward = ibus->ime_switcher_keys[i].keycode != 0`
   (`bus/ibusimpl.c:2622`); `ibus-ui-gtk3` writes `kb.reverse ? 1 : 0` into it
   (`ui/gtk3/panel.vala:549`). Nothing ever compares it against a hardware
   keycode. Putting a real evdev+8 there — the natural reading of `a(uuu)` —
   would silently register every trigger as backward. The same field in the
   *outgoing* `GlobalShortcutKeyResponded` is a genuine keycode: the harness log
   shows `keycode=65` for space, `keycode=37` for Control_L. The wire type is
   symmetric and the meaning is not.
3. **The enum is `IBusBusGlobalBindingType` and it lives in `ibusbus.h:79-84`**
   — `ANY` 0, `IME_SWITCHER` 1, `EMOJI_TYPING` 2 — not in `ibustypes.h` and not
   under a `…GLOBAL_SHORTCUT_KEYS…` name. Only `IME_SWITCHER` is storable:
   the setter's `switch` has one case and a `default` that frees the keys it was
   handed (`bus/ibusimpl.c:2039-2050`), so registering the emoji type in 1.5.34
   stores nothing and fires nothing.
4. **A registration must be spelled in modifiers the daemon can still see when
   it compares.** Before matching, the daemon rewrites `SUPER` to `MOD4` and
   then masks with `IBUS_MODIFIER_FILTER` (`bus/ibusimpl.c:2615-2619`), and that
   filter *excludes* `SUPER`, `HYPER` and `META` (`ibustypes.h:386-398`). A
   trigger registered with `IBUS_SUPER_MASK` can therefore never match anything.
   `<Super>space` — the schema default — has to be registered as `MOD4`. GTK's
   panel gets there by a different road, `gdk_keymap_map_virtual_modifiers`
   (`ui/gtk3/bindingcommon.vala:73-84`); we map `<Super>`/`<Hyper>` to MOD4 and
   `<Meta>` to MOD1 directly, which is where a standard X keymap puts them.
5. **The trigger is consumed before focus, before the engine, and before the
   post-process queue.** `_ic_process_key_event` calls
   `bus_ibus_impl_process_key_event` first and, on a hit, returns `(b) TRUE` and
   clears `processing_key_event` (`bus/inputcontext.c:1085-1099`). So the sync
   drain is empty for a trigger press, the routing rules see `handled=true` and
   swallow it, and no space is typed. It also means *any* context can trigger a
   switch, focused or not.
6. **Every use of a modified trigger fires the signal twice, and the second one
   swallows a key release.** Press gives a hit with the press state; releasing
   the trigger key while the modifiers are held gives no hit; releasing the last
   modifier gives a *second* hit with `RELEASE_MASK` set
   (`bus/ibusimpl.c:2626-2647`, driven by a file-static `binding_state`). That
   is the panel's switcher-popup protocol — press moves the selection, release
   commits it. Cycle-only v1 acts on the press and ignores the release. The
   side effect is worth knowing: because the release edge also answers
   `handled=true`, the frontend swallows that key release and the application
   never sees it. Measured in the harness, `Control_L release` after
   Ctrl+Alt+Space is swallowed. Harmless as things stand — the grab's
   `modifiers` events are forwarded to the virtual keyboard independently, so
   the compositor-level modifier state stays correct — but an application that
   tracks modifiers from key events alone would see one stuck.
7. **Correction to phase-1 finding 9.** "The engine attaches to a context only
   on `FocusIn`" is true of the *first* attach and false of a switch:
   `bus_ibus_impl_set_global_engine_by_name` changes the engine on the focused
   context directly (`bus/ibusimpl.c:1007-1038`), so a `SetGlobalEngine` while
   our context is focused takes effect immediately with no refocus. The harness
   proves it — mozc converts `konnnitiha` right after a switch into `mozc-on`.
8. **The panel role is exclusive in practice even though nothing enforces it.**
   `GlobalShortcutKeys` is last-writer-wins with no unregister, and (finding 1)
   the response is broadcast. So with `ibus-ui-gtk3` running as the session's
   panel, registering would replace its trigger while it is still acting on the
   response, and *both* processes would cycle the engine on every press. The
   nested-display argument does not help here: a display has seats, a daemon
   does not. `im::run` therefore leaves the switcher entirely inert when
   `ibus-ui-gtk3 --enable-wayland-im` is running and no explicit ibus address
   was given, and says so in the log. This is the phase-4 analogue of the
   input-method binding check, and it is the reason phase 4 could be developed
   and tested without an attended session.
9. **Engine cycling reads dconf by shelling out to `gsettings`.** Nothing in the
   dependency graph pulls glib in, and adding `gio` to read three string lists
   at startup would be the largest dependency in the tree by build time. The
   engine list reproduces the panel's own construction: `engines-order`
   intersected with `preload-engines`, then whatever the order does not mention
   (`ui/gtk3/panel.vala:1390-1408`). The panel then keeps that list in MRU order
   with the current engine at index 0 and switches to index 1 or `len - 1`
   (`ui/gtk3/panel.vala:1345-1352`); we do not, because MRU is what makes its
   switcher popup useful and, with no popup, only makes the cycle
   unpredictable. `config.ron`'s `ibus_triggers` and `ibus_engines` override
   both lists, which is what gives the harness known fixtures — the scratch
   daemon runs `--config disable` with an `XDG_CONFIG_HOME` of its own, so its
   dconf is empty and its `preload-engines` with it.
10. **The switch is confirmed, not assumed.** `SetGlobalEngine` returning
    success does not mean the engine changed — the daemon can refuse, and the
    engine can change from elsewhere. `current` is only ever updated from
    `GlobalEngineChanged`, which the daemon emits either way, so the cycle
    cannot drift from what is really in effect. Measured latency in the
    harness: 210ms for the first switch into `mozc-on` (engine process
    startup), ~1ms after that.
11. Minor: registering makes `bus_ibus_impl_is_wayland_session` true
    (`bus/ibusimpl.c:2666-2672`), which arms the `ignore_focus_out` trap of
    phase-1 finding 1 — our client name already opts out, and this is now the
    code path that arms it rather than a future one. Holding the trigger down
    repeats it, because `xkb_keymap_key_repeats` is true for space and the
    frontend's repeat timer does not know the key was a shortcut; a tap is well
    inside the 600ms delay, so it only bites someone who holds the combination.
    Registering an empty list is refused by the daemon
    (`g_return_val_if_fail (size > 0, FALSE)`, `bus/ibusimpl.c:2025`), so
    nothing is sent when no accelerator parsed.

### What was verified, and how

`scripts/im-harness/switcher-test.sh`, all seven assertions green on
2026-08-27 against the scratch daemon: the `(ya(uuu))` `Set` is accepted;
Ctrl+Alt+Space moves `xkb:us::eng` → `mozc-on` and back; the Shift-modified
trigger arrives with `is_backward` set, which is the keycode-slot encoding
making a full round trip; and mozc converts `konnnitiha` afterwards, which it
could not do if the engine had not really changed. `frontend-test.sh` still
passes unchanged, and in that run the panel takes its trigger from the *live*
dconf (read-only) rather than from a config file, which is the other half of
the path.

What still needs a human: the live cutover, unchanged. Phase 4 adds nothing to
that list — the guard in finding 8 means the switcher is inert on the live
daemon until `ibus-ui-gtk3 --enable-wayland-im` is retired, which is the same
moment the input-method binding becomes ours.

## Findings from phase 3 (2026-08-27)

Building the candidate window against the input-method protocol, cosmic-comp's
pinned smithay, ibus 1.5.34's panel and mozc 3.34 turned up these corrections.
Citations are into ibus 1.5.34, smithay `e3d461a` (cosmic-comp's pin) and the
protocol XML in `wayland-protocols-misc 0.3.12`.

1. **`zwp_input_popup_surface_v2` has no configure, no ack, no size request
   and no visibility control.** The whole interface is one event
   (`text_input_rectangle`) and one destructor
   (`input-method-unstable-v2.xml:366-390`). The compositor decides visibility
   — "visible if and only if the input method is in the active state" — and
   derives the popup's *size* from the committed buffer: cosmic-comp calls
   `bbox_from_surface_tree` on the surface and places the result below the
   caret rectangle, clamped right, flipped up if the bottom would overflow
   (`xdg_shell/popup.rs:178-205`). So the client's only levers are the buffer
   and its dimensions. Hiding is therefore a **null-buffer commit** — core
   Wayland unmapping, not a protocol request. Verified working in the harness.

2. **The popup surface must be created once and kept, not per activation** —
   the opposite of the grab and the virtual keyboard, and the doc's suggestion
   that it could go either way is wrong on this compositor. Smithay's
   `activate_input_method` re-registers whatever popup already exists against
   the newly focused text input on *every* activation: dismiss, `set_parent`,
   `new_popup` (`wayland/input_method/input_method_handle.rs:125-143`). So an
   activation-scoped surface buys nothing. Worse, `set_text_input_rectangle`
   only forwards to a popup that exists at the moment the text input updates
   its cursor rectangle (`:103-121`), so a surface created inside `activate`
   misses the rectangle that came with the activation and sits at a stale
   position until the caret next moves. The one hazard of a long-lived surface
   is covered: created before any field has focus, `get_parent()` is `None` and
   smithay skips `new_popup` (`:272-274`), leaving it untracked — and the first
   `activate` registers it anyway.

3. **`UpdateLookupTable` carries the whole candidate list, and the page is
   derived.** The daemon does no slicing at all
   (`bus/inputcontext.c:2764-2777`); `cursor_pos` is a global index; the
   visible page is `[cursor_pos / page_size * page_size, +page_size)` and the
   highlighted row is `cursor_pos % page_size`
   (`ui/gtk3/candidatepanel.vala:339-361`). The `_fast` engine call sends
   **three** pages, not one, with `cursor_pos` renumbered window-relative
   (`src/ibusengine.c:2065-2117`) — and the same formula is correct for that
   too, so there is only one of it. (The "3 candidates, page 3" in the phase-1
   trace is a formatting artefact: `LookupTable`'s `Display` prints `page_size`
   after the word "page". mozc sets `page_size` to the candidate count for
   small tables.)

4. **Labels are indexed by slot within the page, not by candidate, and the
   defaults fill absolutely.** The panel reads `get_label(i)` for
   `i in 0..page_size` (`ui/gtk3/candidatepanel.vala:350-354`) and then fills
   every remaining slot from a fixed table indexed by the same `i`
   (`ui/gtk3/candidatearea.vala:112-118`) — so an engine that supplies three
   labels gets `"4."`, `"5."`, … in slots 3 onward, not a continuation. The
   table is `"1."`…`"9."`, then `"0."`, then `"a."`…`"f."`
   (`ui/gtk3/candidatearea.vala:38-41`); the tenth candidate is `0` because
   that is the key you press. Page size is capped at 16
   (`src/ibuslookuptable.c:224`).

5. **`PageUp`/`PageDown`/`CursorUp`/`CursorDown` carry no payload and are not
   followed by a fresh table.** The daemon mutates its own copy and emits a
   bare signal (`bus/inputcontext.c:2425` and neighbours), returning early
   without emitting if the move fails. A client with `CAP_LOOKUP_TABLE` must
   mirror `ibus_lookup_table_page_up` and friends
   (`src/ibuslookuptable.c:434-518`) **exactly**, quirks included: the rounding
   page-up computes `page_count * page_size + slot` and then clamps, so it
   always lands on the last *candidate* rather than on the same slot of the
   last page. Being bug-compatible is the only way to keep our cursor and the
   daemon's in step, since neither side ever re-syncs.

6. **`ShowLookupTable` and `ShowAuxiliaryText` are dead in IBus's own panel.**
   `ui/gtk3/panel.vala` overrides only the `update_*` and `hide_*` methods, so
   the base class routes the show requests to GObject signals nothing is
   connected to (`src/ibuspanelservice.c:1474-1505`). Visibility there is
   entirely the `visible` flag on `Update*` — and `update(table, false)` is
   implemented as `set_lookup_table(null)`, i.e. identical to `hide`
   (`ui/gtk3/panel.vala:2015-2023`). We honour the show signals anyway: it
   costs a line, the daemon already suppresses a show that changes nothing
   (`bus/inputcontext.c:2364-2366`), and the one sequence it rescues —
   `update(table, false)` then `show()` — is silently broken upstream.
   Auxiliary text with no candidates is a standalone window in the panel
   (`ui/gtk3/candidatepanel.vala:427-444`), and here too; it is how mozc's
   「Tabキーで選択」 appears.

7. **`fc-match sans-serif:lang=ja` is the wrong way to pick the font.** On this
   machine it answers *WenQuanYi Zen Hei*, a Chinese face whose kanji are the
   Chinese glyph variants — no tofu, and still wrong. The renderer names its
   preference list explicitly, Japanese first, and logs which family it got.

8. **The harness was testing the wrong routing, silently.** mozc chooses its
   candidate window at engine startup and the choice is environmental: it looks
   at `WAYLAND_DISPLAY` and `XDG_CURRENT_DESKTOP` (both visible in the engine
   binary's strings), and with `WAYLAND_DISPLAY` unset it concludes X11 and
   drives its own `mozc_renderer`, emitting **no lookup tables at all**. The
   scratch environment deliberately has no `WAYLAND_DISPLAY`, so every
   candidate signal was missing and nothing said so. The fix is
   `MOZC_IBUS_CANDIDATE_WINDOW=ibus` in the scratch environment, which forces
   the IBus path — the same path the live session takes, where
   `WAYLAND_DISPLAY` *is* set and COSMIC is not in
   `compatible_wayland_desktop_names` (`["GNOME"]`) — without handing the
   engine a compositor to reach. Worth carrying as a general hazard: an
   engine's output can depend on environment the harness is deliberately
   withholding.

9. **cosmic-comp does not implement `wlr-screencopy`, so the screenshot gap is
   not one an existing tool closes.** The nested compositor's registry
   advertises `ext_image_copy_capture_manager_v1` with
   `ext_output_image_capture_source_manager_v1` and
   `zcosmic_workspace_image_capture_source_manager_v1`, and no
   `zwlr_screencopy_manager_v1`. Nothing installed here speaks any of them:
   `grim` (wlr-screencopy only) is absent, and `cosmic-screenshot` goes through
   the xdg-desktop-portal Screenshot interface on the *live* session bus, which
   the nested compositor's private bus cannot activate. Automating the visual
   pass therefore means writing an `ext-image-copy-capture` client, which is a
   piece of work rather than a missing package. Until then the candidate
   window's appearance is checked two other ways: the renderer's unit tests
   write PNGs of a fixture table, and a human looks at the nested compositor.

10. **Redraws have to coalesce across a *burst* of signals, not just across
    frames.** One keystroke through mozc produces `UpdateAuxiliaryText` and
    `UpdateLookupTable` together, and committing produces `HideLookupTable`
    then `HideAuxiliaryText`; each arrives as its own callback on the signal
    channel. Painting from each one attached a buffer for a state nobody should
    see — measured: committing a conversion drew a candidate-less window
    containing only the auxiliary line, for the microseconds between the two
    hide signals. Calloop's post-dispatch callback (the third argument to
    `EventLoop::run`, previously `|_| {}`) runs after the whole burst and is the
    right place to draw. Frame callbacks throttle on top of that.

11. Minor: the popup's background is forced opaque even when the COSMIC theme
    asks for translucency. COSMIC's own popovers are translucent *and blurred*,
    and there is no blur available to a client on an input-popup surface, so
    following the theme would mean candidate kana laid over whatever text is
    behind them. Everything is drawn at buffer scale 1 — every output on this
    machine is at scale 1.0, and doing it properly means tracking `wl_output`
    through `wl_surface.enter` and repainting on a move. Both are noted rather
    than solved, as is following the theme live: it is read once, at popup
    creation.

### What was verified, and how

`scripts/im-harness/candidate-test.sh` is the phase-3 regression, all eight
assertions green on 2026-08-27. It runs the frontend at debug level against the
nested compositor and the scratch daemon, injects `konnnitiha`, space and Tab
over libei, and asserts on the frontend's own log: a CJK font and a palette were
chosen, the compositor delivered `text_input_rectangle`, mozc sent a visible
table (nine candidates), a 194×288 buffer was attached and committed, moving the
selection produced another table, the window was unmapped on commit, and the
conversion reached the entry. The caret rectangle tracks the caret across the
field as characters are typed, which is what confirms the popup is anchored to
the caret rather than to the window.

`cargo test` renders four PNGs of fixture tables — vertical, horizontal,
auxiliary-only, and the same fixture through the *configured* theme rather than
the fallback — under `$COSMIC_VOICE_RENDER_DIR` (default the temp dir), which is
how the layout was iterated on without a compositor in the loop.

`frontend-test.sh` and `switcher-test.sh` still pass unchanged.

## Findings from phase 5 (2026-08-27)

Wiring the dictation engine into the frontend, and turning the design's
`multiplexer: false` sketch into a shipped configuration, turned up these
corrections.

1. **"Router states, one per activation" is the wrong scope, and the right one
   is the opposite.** An utterance outlives an activation: the user can be
   speaking when focus moves, and the engine keeps recording either way. So the
   turn is process state, not activation state — and a deactivation
   *terminates* it rather than being the thing that scopes it, dropping back to
   `Forwarding` so that the commit lands on the virtual keyboard instead of
   into whichever window took focus. Reading it as per-activation would have
   left the router in `Dictating` across the focus change, which is exactly the
   state in which a partial becomes a preedit in the wrong application.

2. **The im→engine "answer" cannot be a reply.** The design and the task both
   describe the engine asking whether a text-input client is active. A
   request/reply over a channel either blocks the engine's tokio loop — which
   also drives audio capture — or hands it an answer that was already stale
   when it arrived. The frontend *publishes* the fact instead (two atomics:
   bound, and active), the engine reads it immediately before each use, and the
   residual race is resolved on the other side by the turn-taking function,
   which sees the current value. The two flags are separate because they fail
   separately: a frontend backing off after a crash is not usable even though
   the last activation it saw was real.

3. **The dictation preedit must carry no `ClientCommitPreedit` mode, and that
   is load-bearing rather than incidental.** The frontend remembers the mode an
   engine attached to each preedit, and `Flush` commits one only when it is
   `PREEDIT_COMMIT`. A dictation partial recorded with that mode would be
   flushed by the *next* `Begin` — half a spoken sentence committed as if it
   were a half-typed conversion. `None` is the correct mode for text nobody
   asked us to hold.

4. **The three-way config mode in the failure story should be two-way.** The
   doc imagined `bind_input_method` "evolving into a three-way mode". The third
   state — bind the slot from the injector, without the multiplexer — is the
   thing that caused the 2026-08 incident, and there is no reason to keep it
   reachable. `input_method` is `Off` or `Multiplexer`, and in *both* the
   injector is constructed with `bind_im = false`, so there is no input-method
   object on that connection at all. Enforcing it structurally rather than by
   convention costs one argument and removes a whole class of edit that could
   reintroduce a second binder.

   The retired flag is warned about and ignored rather than mapped to
   `Multiplexer`: an old setting must not activate an exclusive seat resource
   on its own, and the mode should be written by the person doing the cutover.

5. **"Run under a supervisor (systemd user unit)" is right about the mechanism
   and wrong about the reason.** Inside the applet it is a thread and a restart
   loop, and what that buys is not availability — a dead frontend costs nothing,
   because the grab is released by its own object's destructor and key flow is
   restored by the crash itself (phase-2 finding 13). What it buys is
   *visibility*. An input method that is not there looks exactly like one that
   is, right up until a preedit silently fails to appear. The supervisor's real
   output is the line in the popup.

6. **The startup safety check has to be made twice, in two places, for two
   different reasons.** `im::run` refuses to bind while `ibus-ui-gtk3
   --enable-wayland-im` is running; that is the enforcement and it stays. The
   supervisor makes the same `/proc` scan *before* calling it, because a
   refusal from inside `run` is an error string and this state is not an error
   — it is the pre-cutover configuration, it is what every user sees the first
   time they set the mode, and it deserves a sentence naming the fix rather
   than a stack of `Failed` events. The applet turns it into
   "Input method: blocked — …" and a warning icon, and re-checks every five
   seconds so that `ibus exit; ibus-daemon …` is noticed without an applet
   restart.

7. **A commit that arrives outside `Dictating` is honoured, not dropped.** The
   engine decides its path by reading the published flag, so a `Begin` that
   found no field leaves the router in `Forwarding` while the engine may
   nonetheless find a field by the time the transcript is ready. Dropping the
   commit to keep the state machine tidy would throw away the user's sentence;
   committing it puts the sentence in the field they are looking at. The
   partial in the same situation *is* dropped, because a preedit would
   overwrite one mozc believes it owns and there is nothing to lose by waiting
   for the commit.

8. **Phase-2 finding 10's flush is worth what it claimed, and this is the first
   evidence of it.** `ClientCommitPreedit` was set in phase 1 and the flush was
   written in phase 2, guarded on still being active, with the note that "the
   `Reset` path is the one phase 5 actually needs". It is: the harness leaves
   mozc holding an uncommitted こんにちは, sends a `Begin`, and the text lands in
   the field instead of disappearing — which is a data-loss bug in IBus's own
   bridge that we do not have.

9. Minor. `tokio::select!` builds every arm's future at once, so two arms
   cannot both borrow `self`; the frontend's event receiver is taken out of the
   engine into a local for the life of the loop. A calloop channel belongs to
   the loop that polls it, so the frontend creates it and publishes the sending
   end through a link that outlives any one frontend — which is also what makes
   a supervisor restart invisible to the engine. And the `--dictation-fifo`
   reader reopens on end-of-file, because each `echo … > fifo` from a script is
   a writer that opens, writes and closes; it refuses a path that does not
   exist rather than creating one, which would race the script about to write
   to it.

10. **A bare `ibus start` is `--type wayland` on COSMIC.** Found on the first
    live attempt (2026-08-27): the cutover rewrote the autostart to
    `Exec=ibus start`, the manual step ran it, and `ibus-ui-gtk3
    --enable-wayland-im` came straight back with fresh timestamps. In ibus
    1.5.34 `start_daemon_real` (`tools/main.vala:740`) tries the types in
    order — wayland, kde-wayland, systemd, direct — until one succeeds, and
    the wayland attempt succeeds whenever the compositor advertises
    `zwp_input_method_manager_v2` (`registry_global_cb`, `main.vala:226`).
    `--type wayland` only forbids the fallbacks. The only form that cannot
    grow a bridge is running `ibus-daemon` itself with the arguments the
    bridge would have passed it, `--xim --panel disable` (`main.vala:292`),
    which is what the autostart now says and what the manual step runs (with
    `--daemonize`, since it is typed into a terminal). `--type direct` is the
    same thing spelled through `ibus`: it `execv`s `ibus-daemon` with whatever
    options it did not recognise (`main.vala:806`), so the extra layer buys
    nothing but a dependency on that behaviour staying put.

11. **A daemon with no panel has no global engine, and the switcher has to
    choose one.** Second thing the first live attempt showed: the slot bound,
    the popup read "bound, waiting for IBus", `ibus engine` said `No engine is
    set`, and every key fell through unhandled. Choosing the startup engine was
    ibus-ui-gtk3's job — `update_engines` ends in `switch_engine(0, true)`,
    i.e. `SetGlobalEngine` on the head of `engines-order`
    (`ui/gtk3/panel.vala:1445`) — and nothing in the daemon does it for a
    panel-less start. With `use-global-engine` on, no global engine means no
    engine at all, which is silent: the daemon is up, contexts are created,
    `ProcessKeyEvent` simply answers false. The switcher now does what the
    panel did, on connect, only when the daemon reports none (an engine the
    cycle does not list is left alone: it got there by the user's hand and
    `next_engine` already handles an outsider). `switcher-test.sh` no longer
    sets the scratch daemon's engine by hand, so its first assertion is now
    this one.

    Lost with the same process, and not yet replaced: the tray indicator
    (ibus-ui-gtk3's StatusNotifierItem, with the engine icon, mozc's input-mode
    icon via the engine's `icon_prop_key`, and a menu for both). The mode icon
    and the property menu are only reachable by *being* the daemon's panel
    service (`org.freedesktop.IBus.Panel`: `RegisterProperties`,
    `UpdateProperty`, `PropertyActivate`), which the phase-4 panel connection
    deliberately is not. Decided 2026-08-27: replaced in the applet, without
    a panel service — see "Phase 6" at the end of this document.

### What was verified, and how

`scripts/im-harness/dictation-test.sh` is the phase-5 regression, all eight
assertions green on 2026-08-27 against the nested compositor and the scratch
daemon. A fifo stands in for the microphone — `devtest im-frontend
--dictation-fifo` reads newline-delimited JSON `DictationCmd`s and feeds them
in exactly as the engine would — so the whole turn-taking path runs with no
audio, no recogniser and no live session: mozc is left holding an uncommitted
conversion, `Begin` commits it and resets behind it, partials appear as preedit
and revise in place, the transcript commits as text, and typing afterwards
converts again and commits into the same field. The decision itself is eleven
unit tests in `im::dictation`, in the same shape as `im::router`'s.

`frontend-test.sh`, `candidate-test.sh` and `switcher-test.sh` all still pass
unchanged.

What still needs a human: the cutover, unchanged, and now scripted as far as it
safely can be. `scripts/cutover.sh` checks the preconditions, backs up
`~/.config/autostart/ibus-wayland.desktop`, rewrites its one `Exec` line and
then stops and prints the rest — retiring the running bridge with `ibus exit;
ibus-daemon --xim --panel disable --daemonize`, setting the mode, restarting
the applet, and what to do if the
keyboard dies. It deliberately stops and starts nothing: the instant the slot
changes hands is the one worth watching. `scripts/rollback.sh` reverses it, in
the reverse order, which matters — the multiplexer has to let go before IBus's
bridge takes hold.

## Phase 6: the panel presence, in the applet (planned 2026-08-27)

Phase-5 finding 11 ends with what `ibus-ui-gtk3` did that nothing replaced:
the tray indicator with the engine, mozc's input mode, and a menu for both.
Decision: it comes back inside the applet we already have on the panel, and
**not** by owning `org.freedesktop.IBus.Panel`. The user's words: never liked
the panel anyway.

### Principle

Properties are per-context, and the daemon routes them by *capability*, not by
who the panel is. With `IBUS_CAP_PROPERTY` in `SetCapabilities`, the engine's
`RegisterProperties` and `UpdateProperty` are emitted as D-Bus signals on our
input context (`bus/inputcontext.c:2544-2551`, `:2573-2580`), and
`PropertyActivate(s key, u state)` is a method of the context
(`bus/inputcontext.c:1375-1387`, dispatched straight to the context's engine).
Without the capability the same calls go to the panel service, which is the
only reason `ibus-ui-gtk3` ever saw them. So the whole panel role that is
left — the mode glyph and the menu — is one capability bit on the connection
we already hold, plus a codec and some UI.

The glyph rule is `ibus-ui-gtk3`'s (`ui/gtk3/panel.vala:1790-1795`): the
property whose `key` equals the engine's `icon_prop_key` (an `EngineDesc`
field; mozc's is `InputMode`) supplies the indicator through its `symbol`
text, updated by `UpdateProperty` as the mode changes. Everything else in the
list is the menu. mozc re-registers on every FocusIn
(`unix/ibus/property_handler.cc`, `Register`), and xkb engines register
nothing, so the cache is cleared on `GlobalEngineChanged` and refilled by the
next `RegisterProperties` — an engine with no properties has an empty menu,
not a stale one.

### Wire format

`IBusProperty` serialises (`src/ibusproperty.c:362-401`) as
`(sa{sv} s u v s v b b u v v)`: the serializable header, then `key`,
`type`, `label` (variant holding `IBusText`), `icon`, `tooltip` (`IBusText`),
`sensitive`, `visible`, `state`, `sub_props` (variant holding `IBusPropList`),
and — after `sub_props`, "keep the serialized order for the compatibility" —
`symbol` (`IBusText`). `IBusPropList` is `(sa{sv} av)` of property variants.
Types: NORMAL 0, TOGGLE 1, RADIO 2, MENU 3, SEPARATOR 4; states: UNCHECKED 0,
CHECKED 1, INCONSISTENT 2 (`src/ibusproperty.h`). Verify against the source,
not this paragraph, and prove it with a round trip.

### Deliverables

1. `src/ibus/text.rs`: `Property` and `PropList` on the `Serializable`
   trait, with unit tests: a round trip, and a hand-built mozc-shaped list
   (an `InputMode` MENU with RADIO children, one CHECKED, and a `Tool` MENU
   of NORMAL items). Expose `EngineDesc::icon_prop_key` if the codec does not
   already (check the field order in `src/ibusenginedesc.c`).
2. `src/ibus/context.rs`: `CAP_PROPERTY` joins `CAPABILITIES` (and the doc
   comment that says why it was left out is rewritten to say why it is in);
   `ContextSignal::RegisterProperties(PropList)` and
   `UpdateProperty(Property)`; `Context::property_activate(key, state)`.
   `devtest ibus-keys` prints properties as they arrive — safe on the live
   daemon and the fastest way to see what mozc actually sends.
3. `src/im/properties.rs`, pure: the property model — `register(list)`,
   `update(prop)` (by key, recursing into `sub_props`), `clear()`,
   `indicator(icon_prop_key) -> Option<String>`, and a `menu()` view. Unit
   tests in the shape of `im::dictation`'s. The frontend owns one, feeds it
   from the link's signals, clears it on `GlobalEngineChanged`, and publishes
   a snapshot through `ImEvent` whenever it changes.
4. `src/ipc.rs`: everything the applet renders is in `Snapshot`, serde
   types only, no `ibus` types. `InputMethodState::Running` gains the
   engine cycle (described: name, longname, symbol), the indicator, and the
   menu (`ImProperty { key, label, kind, state, sensitive, visible, symbol,
   children }`). Two new commands, `SetEngine(String)` and
   `ActivateProperty { key, state }`, with CLI verbs (`cosmic-voice engine
   <name>` at least) so the harness and scripts can drive them.
5. The engine-to-frontend path: today `DictationLink` carries
   `DictationCmd` into the running frontend's calloop loop. The two new
   commands need the same road. Generalise the link rather than adding a
   second one, but leave `im::dictation::advance` and its tests untouched:
   the turn-taking decision is not what is changing. `SetEngine` goes to the
   switcher (`set_global_engine`, confirmation via `GlobalEngineChanged` as
   always); `ActivateProperty` goes to the context.
6. `src/app.rs`: the panel button shows the mic icon and, when the
   multiplexer is running with an indicator, the glyph beside it; the ready
   line prefers the live indicator over the engine's static symbol. The popup
   gains an "Input method" section: one row per engine in the cycle with the
   current one marked (press to switch), then the menu — a MENU of RADIO
   children as a titled group of selectable rows, a TOGGLE as a toggler, a
   NORMAL as a button, SEPARATOR as spacing; `visible: false` hidden,
   `sensitive: false` disabled; `label` text, key as fallback. Nothing in the
   popup may block: commands go through the engine handle as the existing
   ones do.
7. `scripts/im-harness/property-test.sh`, against the scratch daemon with
   `mozc-on`: properties registered with an `InputMode` menu; indicator is
   mozc's hiragana glyph; `ActivateProperty InputMode.Direct` → an
   `UpdateProperty` changes the indicator and `konnnitiha` then arrives as
   ASCII; `InputMode.Hiragana` converts again; `SetEngine xkb:us::eng` clears
   the menu and the indicator; back to `mozc-on` re-registers. Drive it
   through the fifo `devtest im-frontend` already reads for dictation,
   generalised to a control fifo (rename the flag or alias it; the four
   existing suites stay green either way).
8. `docs/multiplexer.md` "Findings from phase 6", README (the cutover
   section says what the popup now shows and that the tray icon is gone on
   purpose), and the usual: `cargo test`, `cargo build` before any harness
   run, all five suites, nothing bound on the live display.
