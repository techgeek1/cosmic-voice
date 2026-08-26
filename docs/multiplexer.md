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
- The autostart changes from `ibus start --type wayland` to plain
  `ibus start`. ibus-daemon, mozc, and `--xim` (XWayland clients) all stay.
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

## Turn-taking (the point of all this)

Router states, one per activation:

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
  Re-enabling IBus's own bridge is one autostart-line revert away.

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
   migration, applet status surface.

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
2. **Candidate paint stack**: tiny-skia + cosmic-text (recommended: small,
   no toolkit, we control the surface) vs embedding iced (heavy; iced cannot
   target an IM popup surface without surgery).
3. **Engine switcher UX**: cycle-only v1 (recommended) vs popup list.
4. **Upstream fix**: file the smithay `add_instance` bug with the incident
   as the repro narrative. Costs an afternoon, benefits everyone who ever
   binds this protocol on COSMIC. Recommended.

## Build strategy (banked 2026-08-25)

Status: **phase 1 done (2026-08-26)**, `src/ibus/` — see the findings
section below for what implementing it corrected in this document. Phases
2–5 not started.

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
