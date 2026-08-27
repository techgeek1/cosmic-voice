# Input-method test harness

Everything needed to exercise the phase-2 Wayland frontend (the
`zwp_input_method_v2` binder) without touching the live session: a nested
cosmic-comp, fully automated key injection into it over libei, a
text-input-v3 client to observe commits and preedit, and an isolated
ibus-daemon + mozc.

Verified working on 2026-08-26 against cosmic-comp 1.6.0 (`314fc670`),
libei 1.6.0, gtk3 3.24.52, ibus 1.5.34, mozc 3.34.

## Safety invariants

Read these before changing anything here.

1. **Nothing in this directory may talk to the live compositor.**
   `nested-comp.sh` publishes an *absolute* socket path as
   `WAYLAND_DISPLAY` and `lib.sh:harness_assert_not_live` refuses any path
   that resolves to `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY` of the live session.
   Never run a harness client with a bare display name.
2. **Nothing may bind `zwp_input_method_v2` on the live seat.** The frontend's
   devtest refuses the live display unless given `--live-i-know`. Nobody
   should ever pass it from a script; that flag exists for an attended
   session with a recovery shell open. Binding the live slot while ibus holds
   it kills the session keyboard (see `docs/multiplexer.md`, "Why the slot
   cannot be shared").
3. **The nested compositor gets a private D-Bus session bus.** cosmic-comp
   calls `request_name("com.system76.CosmicComp")` unconditionally; on the
   live bus that races with the live compositor for the name that the portal
   and the OSK use to obtain EI sockets. The private bus also has *no service
   directories*, so nothing can be D-Bus-activated on it - in particular
   `org.freedesktop.IBus`, which would otherwise spawn a daemon against the
   user's real `$HOME`.
4. **The scratch ibus never shares state with the live one.**
   `~/.config/mozc` and `~/.config/ibus` are *copied*, never linked; the
   copied `bus/` address files (live daemon address) and mozc's
   `.session.ipc` / `.server.lock` (live `mozc_server` socket) are deleted
   from the copy, otherwise the scratch engine would attach to the live
   `mozc_server` and share the user's dictionary and history.
   Never run `ibus engine`, `ibus restart`, `ibus exit` or `ibus write-cache`
   without `IBUS_ADDRESS` pointing at the scratch daemon - use
   `scratch-ibus.sh run ...`, which sets it for you.
5. **Everything runs in its own process group** (`setsid`), and every `stop`
   kills the group and then verifies with `ps`. Do not background a harness
   process without recording its pgid.
6. State lives in `$IM_HARNESS_STATE` (default `/tmp/im-harness-$UID`). It
   must be a **short** path: the nested wayland socket and two D-Bus sockets
   live under it and `sun_path` is 108 bytes.

## Automated parts

    scripts/im-harness/frontend-test.sh

The one that matters: the whole phase-2 loop, end to end, with assertions.
Starts everything, binds `cosmic-voice devtest im-frontend` to the nested
display, and checks mozc conversion, commit-as-text, modified-key passthrough,
key repeat and daemon-loss passthrough. Exit status is the result:

    PASS: mozc committed こんにちは
    PASS: xkb engine committed plain text
    PASS: Ctrl-A reached the application
    PASS: key repeat fired 23 times in 1.5s
    PASS: the repeats reached the entry
    PASS: the frontend survived ibus-daemon dying
    PASS: keys pass through raw with no daemon

Needs `cargo build` first. **Never add a `pkill -f` to anything in here**: this
directory's own command lines contain every pattern you would want to match,
and the user's panel applet is also called `cosmic-voice`. Kill by recorded
pid, as the scripts do.

    scripts/im-harness/switcher-test.sh

The phase-4 panel duties, end to end. Same scaffolding as `frontend-test.sh`,
plus a config file passed with `devtest im-frontend --config` that pins
`ibus_triggers` to `["<Control><Alt>space"]` and `ibus_engines` to
`["xkb:us::eng", "mozc-on"]` — neither the scratch daemon's dconf (empty, it
runs `--config disable`) nor the frontend's inherited dconf (the user's real
settings) is a fixture, so the override is what makes exact assertions
possible. Injects the trigger over libei and asserts on the daemon's global
engine:

    PASS: trigger registered: control|mod1+space, shift|control|mod1+space
    PASS: the cycle starts on xkb:us::eng
    PASS: Ctrl+Alt+Space switched to mozc-on
    PASS: pressing it again switched back
    PASS: Shift+Ctrl+Alt+Space reported the backward binding
    PASS: the backward trigger also switched the engine
    PASS: mozc converted after the switch

Ctrl+Alt+Space rather than the schema default `<Super>space` because
cosmic-comp filters its own compositor shortcuts before the input-method grab
sees them, and Super combinations are where those live. With a two-entry cycle
forward and backward land on the same engine, so the *direction* is a unit test
(`im::switcher::tests::cycles_both_ways`) and what this asserts is that the
backward registration exists and that the flag — which travels in the
`a(uuu)` keycode slot — survives the round trip.

    scripts/im-harness/smoke-test.sh ["text to type"]

The harness checking itself, without the frontend in the loop:

Cold-starts everything, injects keystrokes over libei, and asserts that a GTK
entry in the nested compositor received them and that its `zwp_text_input_v3`
object was activated. Exit status is the result. Expected output:

    PASS: entry received 'hello world' from libei injection
    PASS: client got zwp_text_input_v3.enter
    PASS: client sent zwp_text_input_v3.enable

The pieces, if you want them individually:

### `nested-comp.sh` - nested cosmic-comp

    scripts/im-harness/nested-comp.sh start    # prints the client env
    scripts/im-harness/nested-comp.sh env      # eval "$(... env)"
    scripts/im-harness/nested-comp.sh log 100
    scripts/im-harness/nested-comp.sh stop

Starts a private `dbus-daemon`, then `/usr/bin/cosmic-comp --no-xwayland`
with `COSMIC_BACKEND=winit`, a private `XDG_RUNTIME_DIR`, and the live socket
reachable only through a symlink named `host-wayland`. A compositor window
opens on the live desktop; that is expected.

The compositor does **not** log its socket name in this build, so the display
is discovered by looking for the `wayland-*` socket that appears in the
private runtime dir (which starts out containing only `dbus` and
`host-wayland`). It is normally `wayland-1` *inside the private runtime dir* -
same basename as the live session's socket, different directory, which is why
the harness always uses absolute socket paths.

`systemd --user import-environment` and the D-Bus
`UpdateActivationEnvironment` call are gated on the KMS backend in
cosmic-comp, so a nested (winit) instance cannot clobber the live session's
`WAYLAND_DISPLAY`. `COSMIC_SESSION_SOCK` is deliberately not passed through.

### `ei-inject` - key injection (build with `build-ei-inject.sh`)

    scripts/im-harness/build-ei-inject.sh
    scripts/im-harness/ei-inject --bus "$HARNESS_DBUS_ADDRESS" \
        type "hello" key 28 sleep 200

cosmic-comp does **not** listen on an EIS socket and ignores `$LIBEI_SOCKET`.
The only way in is a D-Bus method on the compositor's own bus name:

    com.system76.CosmicComp  /com/system76/CosmicComp/Ei
    com.system76.CosmicComp.Ei.GetSenderSocket(u device_types) -> h fd

`device_types` is the XDG RemoteDesktop bitmask (1 keyboard, 2 pointer,
4 touchscreen); the fd is one end of a socketpair, so the client uses
`ei_setup_backend_fd()`. Access is normally restricted to owners of
`org.freedesktop.impl.portal.desktop.cosmic` or `com.system76.CosmicOSK`;
`ei-inject` requests the latter, and `nested-comp.sh` additionally sets
`COSMIC_ENFORCE_DBUS_OWNERS=0`.

Keys injected this way go through `State::inject_ei_key` ->
`inject_source_key` -> `KeyboardHandle::input_from_source`, i.e. the shortcut
filter *and* whatever keyboard grab is installed - which is exactly the
input-method grab the frontend will hold. virtual-keyboard-v1 keys bypass
that path, which is why the harness uses libei.

Keycodes are evdev codes; `type` maps ASCII on a US keymap (the nested
compositor's default). `utf8`/`keysym` drive cosmic's `ei_text` device
extension.

### `entry-client.py` / `run-entry-client.sh` - the text-input-v3 client

    scripts/im-harness/run-entry-client.sh [title]
    WAYLAND_DEBUG=1 scripts/im-harness/run-entry-client.sh   # protocol trace

A GTK3 window with one `Gtk.Entry`, printing `READY`, `ENTRY <text>`,
`PREEDIT <text>` and `KEY <name>` line-buffered on stdout. The launcher forces
`GDK_BACKEND=wayland` and `GTK_IM_MODULE=wayland` (gtk3's `im-wayland.so`,
the zwp_text_input_v3 implementation) and clears the environment otherwise, so
`QT_IM_MODULE`/`XMODIFIERS`/an inherited `GTK_IM_MODULE=ibus` cannot route the
entry to an ibus daemon instead.

**A text-input-v3 client only becomes active when the compositor has an input
method.** gtk3's `imwayland.c` sends `enable` from `text_input_enter`, and
smithay only sends `enter` when a `zwp_input_method_v2` is bound (or when
cosmic-comp has set `set_compositor_input_method(true)`, which it does while
an EI keyboard connection is active and no real IME is bound). So:

* with nothing else running, the client binds `zwp_text_input_manager_v3` and
  calls `get_text_input`, and that is all you will see - **this is not a bug**;
* hold an `ei-inject ... hold N` connection open, or run the frontend, and the
  client gets `enter` and starts sending `enable` / `set_surrounding_text` /
  `set_content_type` / `set_cursor_rectangle` / `commit`.

### `scratch-ibus.sh` - isolated ibus-daemon + mozc

    scripts/im-harness/scratch-ibus.sh start          # prints IBUS_ADDRESS
    scripts/im-harness/scratch-ibus.sh engine mozc-on
    scripts/im-harness/scratch-ibus.sh run ibus list-engine
    scripts/im-harness/scratch-ibus.sh run ./target/debug/cosmic-voice devtest ibus-info
    scripts/im-harness/scratch-ibus.sh status
    scripts/im-harness/scratch-ibus.sh stop

Runs `ibus-daemon --panel disable --config disable --emoji-extension disable
--address unix:tmpdir=<scratch>/sock --cache refresh` on a private session bus
with `HOME`/`XDG_*_HOME` under the harness tree.

* `--panel disable` keeps `ibus-ui-gtk3` out, so nothing here can bind the IM
  slot.
* `--config disable` keeps the dconf config module out, so the live dconf is
  neither read nor written. Consequence: `preload-engines` is empty on the
  scratch daemon and `registry` reports `0 active` engines; use
  `scratch-ibus.sh engine <name>` (`SetGlobalEngine`) to select one.
* There is no `--xim=no`; `-x/--xim` is a flag, and omitting it means no XIM
  server.
* ibus-daemon exits when its parent process dies, so it is started under a
  `sh -c '... & wait'` inside the new session; killing the process group takes
  both down.
* `ibus engine <name>` exits 1 even on success, so the script reads the engine
  back and fails loudly if it did not take.
* `mozc_server` daemonises and its argv says nothing about which profile it
  serves, so it outlives the engine that started it; `stop` matches it on
  `HOME=` in `/proc/<pid>/environ` and reaps it. Never match it on argv - that
  would hit the live server.

Isolation check that should always hold while it runs: two `mozc_server`
processes, one with `HOME=/home/<user>` (live) and one with
`HOME=$IM_HARNESS_STATE/ibus/home` (scratch).

## Attended parts

These need a human at the keyboard and are deliberately not scripted:

* **Candidate window / preedit appearance.** The nested compositor renders on
  the live desktop; look at its window. There is no scripted screenshot yet.
* **Anything binding the live seat's IM slot** - the phase-2 cutover, the
  visual pass, final acceptance. Keep a recovery shell open. `pkill
  cosmic-voice` must restore key flow; if it does not, kill ibus.
Engine-switch hotkeys are no longer on this list. `switcher-test.sh` covers
them against the scratch daemon, and the frontend refuses to register anything
on a daemon whose panel is still `ibus-ui-gtk3` unless an explicit ibus address
is given — the registration is a last-writer-wins global with no unregister,
and the daemon broadcasts the response to every subscriber, so two panels
would both act on one press. Registering also arms ibus's `ignore_focus_out`
trap (`docs/multiplexer.md`, phase-1 finding 1); our client name opts out of
it, but nothing else on the live daemon would.

## Known gaps

* No screenshot capture from the nested compositor.
* The nested compositor logs `[EGL] 0x300d (BAD_SURFACE) eglQuerySurface` once
  at startup under winit; it is harmless, rendering works.
* The scratch daemon's `devtest ibus-info` prints `daemon pid -` because the
  pid is only discoverable from an address file the harness does not let it
  find; compare the `address` line instead.
