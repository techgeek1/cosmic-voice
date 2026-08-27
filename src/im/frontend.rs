//! The Wayland half of the multiplexer: one process holding the seat's
//! `zwp_input_method_v2` slot and putting an IBus engine behind it.
//!
//! Everything here is driven by one calloop loop on one thread, with three
//! sources: the Wayland connection, a channel carrying IBus's asynchronous
//! signals from the thread that blocks on them ([`super::link`]), and a timer
//! that does key repeat and reconnection. The synchronous `ProcessKeyEvent`
//! call is made inline from that loop and blocks it for the engine's
//! round-trip, which is deliberate — the whole point of the synchronous key
//! path is that no key is routed until the engine has answered for it.
//!
//! # The three lifetimes
//!
//! - **The input method** lives as long as the process. It is bound once and
//!   never rebound; there is no `unavailable` to recover from under the
//!   compositor we target (see `docs/multiplexer.md`).
//! - **The grab and the virtual keyboard** live one activation each, created
//!   when a text field takes focus and destroyed when it loses it. That
//!   matches what IBus's bridge does, matches smithay's own lifecycle, and
//!   bounds the blast radius of a crash: the grab object's destructor is what
//!   releases the seat-wide grab, so dying always restores key flow.
//! - **The IBus context** outlives activations, taking and giving up focus
//!   rather than being rebuilt, which is what keeps a half-typed conversion
//!   alive across a focus change. It dies only when the daemon does.
//!
//! # Ordering
//!
//! Every text operation is its own `commit(serial)`, as the protocol's
//! double-buffering requires and as IBus's bridge does. That has a consequence
//! worth knowing: a `commit` with no `set_preedit_string` beside it clears the
//! preedit, because unset pending state means empty. Engines emit
//! commit-then-preedit, never the reverse, so the net effect is right — but a
//! future engine that emitted them the other way round would lose its preedit.

use anyhow::{Context as _, Result, anyhow, bail};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal, channel};
use calloop_wayland_source::WaylandSource;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2::{self, ZwpInputMethodKeyboardGrabV2},
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

use super::content_type::ContentType;
use super::keyboard::{Keyboard, RepeatInfo, XKB_KEYCODE_OFFSET};
use super::link::{Link, Upstream};
use super::popup::Popup;
use super::router::{self, KeyFacts, Route};
use crate::ibus::{ContextSignal, PREEDIT_COMMIT, PostRecord, RELEASE_MASK, Text, describe_state};

/// How often the loop wakes up when it has nothing else to do.
///
/// Only the reconnection path needs it: if ibus-daemon was not there when a
/// field took focus, this is what notices it came back without waiting for the
/// next keystroke.
const TICK: Duration = Duration::from_secs(3);

// --- Options ---

/// What the frontend needs to know before it binds anything.
pub struct Options {
    /// The Wayland display to bind on, e.g. `wayland-2`. Explicit rather than
    /// inherited from the environment on purpose — see [`run`].
    pub display     : String,
    /// An ibus bus address overriding discovery, so a harness can point this
    /// at a scratch daemon.
    pub ibus_address: Option<String>,
    /// Permission to bind the display this process inherited, which is
    /// normally the user's live session. See [`run`] for why that is gated.
    pub allow_live  : bool,
}

// --- State ---

/// The text around the caret, as the compositor last described it.
///
/// Offsets are byte indices into `text`, which is what the Wayland protocol
/// uses; IBus wants character offsets, so everything that leaves here converts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Surrounding {
    /// The text itself, up to 4000 bytes of it by protocol.
    text  : String,
    /// Byte offset of the caret.
    cursor: u32,
    /// Byte offset of the other end of the selection.
    anchor: u32,
}

/// The half of the activation state that has not been applied yet.
///
/// `activate`, `deactivate`, `surrounding_text`, `text_change_cause` and
/// `content_type` are all pending state; `done` applies the lot atomically.
/// Treating them as immediate is the classic bug in this protocol — a preedit
/// sent between `activate` and `done` goes nowhere.
#[derive(Debug, Clone, Default)]
struct Pending {
    /// Whether a text field wants us, per the last activate/deactivate.
    active      : bool,
    /// The text around the caret, if the client sent any.
    surrounding : Option<Surrounding>,
    /// Why the surrounding text last changed. Cached because engines that do
    /// reconversion care whether the user moved the caret or we did.
    change_cause: u32,
    /// What kind of field it is, already translated for IBus.
    content     : ContentType,
}

/// The objects that exist only while a text field has focus.
struct Session {
    /// Our exclusive claim on the seat's keyboard.
    grab      : ZwpInputMethodKeyboardGrabV2,
    /// Where keys we decline go. One per activation, like the grab, because
    /// the keymap arrives on the grab and a virtual keyboard cannot be used
    /// before it has one.
    vkbd      : ZwpVirtualKeyboardV1,
    /// Whether a keymap has been forwarded to `vkbd` yet. Sending a key or a
    /// modifier set before one has been is a protocol error on wlroots and
    /// undefined elsewhere, so it is gated rather than hoped about.
    has_keymap: bool,
}

/// A preedit we have shown and are still holding.
struct Preedit {
    /// What is on screen.
    text: String,
    /// What the engine says to do with it if focus is lost. Only
    /// [`PREEDIT_COMMIT`] obliges us to do anything.
    mode: Option<u32>,
}

/// A key being repeated.
struct Repeat {
    /// The evdev code being repeated.
    evdev     : u32,
    /// Which scheduling this belongs to. Timers cannot be cancelled from
    /// inside their own callback, so cancellation is expressed by bumping this
    /// and letting the stale timer notice it is stale and drop itself.
    generation: u64,
}

/// The whole frontend: Wayland state, IBus state, and the routing between.
pub struct Frontend {
    /// Handle for creating protocol objects.
    qh          : QueueHandle<Frontend>,
    /// The seat we are the input method for.
    seat        : wl_seat::WlSeat,
    /// Kept to make a virtual keyboard per activation.
    vk_manager  : ZwpVirtualKeyboardManagerV1,
    /// The input method itself, bound once for the life of the process.
    im          : ZwpInputMethodV2,

    /// Pending activation state, applied on `done`.
    pending     : Pending,
    /// Whether a text field has focus right now.
    active      : bool,
    /// Number of `done` events seen, which is the serial every `commit` must
    /// carry.
    serial      : u32,
    /// The applied surrounding text.
    surrounding : Option<Surrounding>,
    /// Why it last changed. Cached and logged rather than relayed, because
    /// IBus has nowhere to put it: `SetSurroundingText` takes a text and two
    /// offsets and nothing else, and IBus's own bridge leaves
    /// `input_method_text_change_cause_v2` an empty function. It is the one
    /// piece of information an engine doing reconversion would want and
    /// cannot have, so it is kept where a later protocol version could use it.
    change_cause: u32,
    /// The applied content type.
    content     : ContentType,

    /// Grab and virtual keyboard, while a field has focus.
    session     : Option<Session>,
    /// The candidate window. Its surface outlives every activation; see
    /// [`super::popup`] for why. `None` only if it could not be created, in
    /// which case everything else still works and mozc converts blind.
    popup       : Option<Popup>,
    /// Keymap and modifier state.
    keyboard    : Keyboard,
    /// The preedit we are holding, if any.
    preedit     : Option<Preedit>,
    /// Whether an engine has ever asked for the text around the caret.
    ///
    /// IBus's bridge keeps the same flag on the context
    /// (`ibus_input_context_needs_surrounding_text`) and it is what turns a
    /// one-off answer into a subscription: an engine doing reconversion needs
    /// the text as it changes, not as it was when it first asked. Sticky for
    /// the life of the process rather than the activation, because an engine
    /// that needs surrounding text in one field needs it in the next one too.
    wants_around: bool,

    /// The key being repeated, if any.
    repeat      : Option<Repeat>,
    /// Monotonically increasing tag for repeat scheduling.
    generation  : u64,
    /// Timestamp of the last real key event, and when it arrived, so that
    /// synthesised keys carry timestamps on the compositor's clock rather than
    /// on one of our own.
    last_key    : (u32, Instant),

    /// The connection to ibus-daemon and the context on it.
    link        : Link,
    /// Where the signal thread sends; cloned for each new context.
    signals     : channel::Sender<Upstream>,

    /// For scheduling repeat timers.
    handle      : LoopHandle<'static, Frontend>,
    /// For stopping the loop when the input method is taken away.
    stop        : LoopSignal,
}

// --- Entry point ---

/// Runs the frontend until the process is killed or the slot is taken away.
///
/// `options.display` is a required argument rather than `$WAYLAND_DISPLAY`
/// because of what binding the wrong one costs. `zwp_input_method_v2` is
/// exclusive per seat, and on the compositor we target a second binder does
/// not merely fail: smithay's `add_instance` sends `unavailable` to the
/// *existing* holder, which then destroys its input method but not its
/// keyboard grab, and every key on the session routes into a grab nobody
/// services until that process is killed. So the display is named explicitly,
/// and binding the one this process inherited — the live session — has to be
/// asked for out loud.
pub fn run(options: Options) -> Result<()> {
    let inherited = std::env::var("WAYLAND_DISPLAY").ok();
    let live = inherited.as_deref() == Some(options.display.as_str());
    if live && !options.allow_live {
        bail!(
            "refusing to bind the input method on {}, which is this process's own \
             WAYLAND_DISPLAY: binding the live seat's input-method slot is an attended \
             operation. Start a nested compositor and name its display instead.",
            options.display,
        );
    }

    match ibus_wayland_bridge() {
        Some(pid) if live => bail!(
            "ibus-ui-gtk3 --enable-wayland-im is running as pid {pid} and already holds this \
             seat's input method. Binding a second one wedges the keyboard session-wide.",
        ),
        Some(pid) => tracing::warn!(
            "ibus-ui-gtk3 --enable-wayland-im is running as pid {pid}; harmless here because \
             {} has a seat of its own, but it would not be on the live display",
            options.display,
        ),
        None => {}
    }

    let conn = connect(&options.display)?;
    let (globals, queue) = registry_queue_init::<Frontend>(&conn)
        .context("initialising the registry")?;
    let qh = queue.handle();

    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=9, ())
        .context("binding wl_seat")?;
    let im_manager: ZwpInputMethodManagerV2 = globals
        .bind(&qh, 1..=1, ())
        .context("binding zwp_input_method_manager_v2")?;
    let vk_manager: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("binding zwp_virtual_keyboard_manager_v1")?;
    // For the candidate window: a surface to give the popup role to, and the
    // shared memory its pixels live in. Both are core globals every compositor
    // has, so failing to bind either means something is very wrong.
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .context("binding wl_compositor")?;
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .context("binding wl_shm")?;
    // `im_manager` stays a live local for the whole of `run`: the manager is
    // not needed again, but a bound global that goes out of scope is one more
    // thing to reason about on a connection whose lifetime is the process's.
    let im = im_manager.get_input_method(&seat, &qh, ());
    tracing::info!("input method bound on {}", options.display);

    // Created here rather than per activation: smithay re-parents an existing
    // popup on every `activate`, and only delivers the caret rectangle to a
    // popup that already exists. See [`super::popup`].
    let popup = Popup::new(&compositor, &shm, &im, &qh)
        .map_err(|e| tracing::error!("no candidate window ({e:#}); conversion will be blind"))
        .ok();

    let mut event_loop: EventLoop<'static, Frontend> =
        EventLoop::try_new().context("creating the event loop")?;
    let handle = event_loop.handle();
    let (sender, receiver) = channel::channel::<Upstream>();

    let mut frontend = Frontend {
        qh          : qh,
        seat        : seat,
        vk_manager  : vk_manager,
        im          : im,
        pending     : Pending::default(),
        active      : false,
        serial      : 0,
        surrounding : None,
        change_cause: 0,
        content     : ContentType::default(),
        session     : None,
        popup       : popup,
        keyboard    : Keyboard::new(),
        preedit     : None,
        wants_around: false,
        repeat      : None,
        generation  : 0,
        last_key    : (0, Instant::now()),
        link        : Link::new(options.ibus_address),
        signals     : sender,
        handle      : handle.clone(),
        stop        : event_loop.get_signal(),
    };

    WaylandSource::new(conn, queue)
        .insert(handle.clone())
        .map_err(|e| anyhow!("registering the wayland source: {e}"))?;
    handle
        .insert_source(receiver, |event, _, frontend| {
            if let channel::Event::Msg(upstream) = event {
                frontend.on_upstream(upstream);
            }
        })
        .map_err(|e| anyhow!("registering the ibus signal channel: {e}"))?;
    handle
        .insert_source(Timer::from_duration(TICK), |_, _, frontend| {
            frontend.on_tick();
            TimeoutAction::ToDuration(TICK)
        })
        .map_err(|e| anyhow!("registering the tick timer: {e}"))?;

    // The closure runs after every dispatch, which is where the candidate
    // window draws: one keystroke through mozc produces a burst of signals and
    // this is the point at which all of them have been applied. See
    // [`Popup::flush`].
    event_loop
        .run(None, &mut frontend, |frontend| {
            if let Some(popup) = frontend.popup.as_mut() {
                popup.flush();
            }
        })
        .context("running the event loop")?;

    Ok(())
}

/// Connects to a named Wayland display.
///
/// `Connection::connect_to_env` would read `$WAYLAND_DISPLAY`, which is
/// exactly the value we are refusing to trust, so the socket is opened by
/// hand. An absolute name is taken as a path, matching how libwayland resolves
/// `WAYLAND_DISPLAY`.
fn connect(display: &str) -> Result<Connection> {
    let path = if display.starts_with('/') {
        std::path::PathBuf::from(display)
    } else {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .context("XDG_RUNTIME_DIR is unset, so a display name cannot be resolved")?;
        std::path::Path::new(&runtime).join(display)
    };

    let stream = UnixStream::connect(&path)
        .with_context(|| format!("connecting to {}", path.display()))?;

    Connection::from_socket(stream).context("starting the wayland connection")
}

/// Finds a running IBus Wayland bridge, which would already hold a slot.
///
/// By scanning `/proc` rather than by asking the compositor, because there is
/// no way to ask: the protocol has no "who holds this" query, and the only
/// probe available — binding it — is the thing that breaks. The cmdline is
/// NUL-separated, so `--enable-wayland-im` is matched as a whole argument.
fn ibus_wayland_bridge() -> Option<u32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let mut arguments = cmdline.split(|byte| *byte == 0);
        let Some(program) = arguments.next() else {
            continue;
        };
        if !program.ends_with(b"ibus-ui-gtk3") {
            continue;
        }
        if arguments.any(|argument| argument == b"--enable-wayland-im") {
            return Some(pid);
        }
    }

    None
}

// --- Activation lifecycle ---

impl Frontend {
    /// The candidate window, for the dispatch handlers that live beside it.
    ///
    /// `None` when the popup could not be created at all, which is a state the
    /// rest of the frontend is designed to survive: keys still route, mozc
    /// still converts, the user just cannot see the candidates.
    pub(super) fn popup(&mut self) -> Option<&mut Popup> {
        self.popup.as_mut()
    }

    /// Applies the pending state. Called once per `done`.
    fn on_done(&mut self) {
        self.serial = self.serial.wrapping_add(1);

        let was_active = self.active;
        self.active = self.pending.active;
        self.surrounding = self.pending.surrounding.clone();
        self.change_cause = self.pending.change_cause;
        let content_changed = self.content != self.pending.content;
        self.content = self.pending.content;

        match (was_active, self.active) {
            (false, true) => self.activate(),
            (true, false) => self.deactivate(),
            (true, true)  => {
                if content_changed {
                    self.push_content_type();
                }
                // The caret moved or the text changed under it. Only an engine
                // that has asked for this wants to hear about it, and only
                // then is the round trip worth making on every keystroke.
                if self.wants_around {
                    self.push_surrounding();
                }
            }
            _ => {}
        }
    }

    /// A text field took focus: take the keyboard and take IBus focus.
    fn activate(&mut self) {
        tracing::info!("activate (serial {}) {}", self.serial, self.content);

        self.preedit = None;
        // The new grab re-sends the keymap, and a keymap parsed from the last
        // grab may describe a layout the user has since changed away from.
        self.keyboard.forget();

        let vkbd = self
            .vk_manager
            .create_virtual_keyboard(&self.seat, &self.qh, ());
        let grab = self.im.grab_keyboard(&self.qh, ());
        self.session = Some(Session {
            grab      : grab,
            vkbd      : vkbd,
            has_keymap: false,
        });
        if let Some(popup) = self.popup.as_mut() {
            popup.set_active(true);
        }

        self.attach_context();
    }

    /// Focus went away: give the keyboard and IBus focus back.
    fn deactivate(&mut self) {
        tracing::info!("deactivate (serial {})", self.serial);

        self.cancel_repeat();
        if let Some(popup) = self.popup.as_mut() {
            popup.set_active(false);
        }
        // A no-op on this path by design, because by now there is no field to
        // commit into and the compositor would give the text to the next one.
        // Called anyway so the rule lives in one place: see the method.
        self.flush_held_preedit();

        self.with_context("Reset", |context| context.reset());
        self.with_context("FocusOut", |context| context.focus_out());

        if let Some(session) = self.session.take() {
            session.grab.release();
            session.vkbd.destroy();
        }
        self.keyboard.forget();
        self.preedit = None;
    }

    /// Gives IBus focus and tells it about the field, building the context
    /// first if the daemon has come back since the last attempt.
    fn attach_context(&mut self) {
        if !self.link.ensure(&self.signals) {
            return;
        }

        // Engines have never been given a zero caret rectangle by GTK and some
        // log about the capability being declared but never answered. Under
        // Wayland we have no screen coordinates to give — cosmic-comp
        // positions the candidate popup from the client's own caret rectangle
        // — so this is a formality, made once.
        self.with_context("SetCursorLocation", |context| {
            context.set_cursor_location(0, 0, 0, 0)
        });
        self.with_context("FocusIn", |context| context.focus_in());
        self.push_content_type();
        self.push_surrounding();

        if let Some(engine) = self.link.global_engine() {
            tracing::info!("ibus engine {engine}");
        }
    }

    /// Periodic work: nothing but reconnection.
    fn on_tick(&mut self) {
        if self.active && self.link.context().is_none() {
            self.attach_context();
        }
    }
}

// --- Keys ---

impl Frontend {
    /// A key arrived from the grab.
    fn on_key(&mut self, time: u32, evdev: u32, pressed: bool) {
        self.last_key = (time, Instant::now());
        // Any edge ends the previous repeat: a release because the key is up,
        // a press because it is a different key now.
        self.cancel_repeat();

        self.deliver_key(time, evdev, pressed);

        if pressed {
            self.schedule_repeat(evdev);
        }
    }

    /// Routes one key press or release, with no repeat bookkeeping.
    ///
    /// Split out so that a repeat can re-run exactly the same path without
    /// cancelling the repeat it is part of.
    fn deliver_key(&mut self, time: u32, evdev: u32, pressed: bool) {
        if !self.keyboard.ready() {
            // No keymap means nothing can be resolved, and a key nobody can
            // read is better delivered than dropped.
            tracing::debug!("key {evdev} before any keymap; passing through");
            self.replay(time, evdev, pressed);
            return;
        }

        let keysym = self.keyboard.keysym(evdev);
        let character = self.keyboard.character(evdev);
        let modifiers = self.keyboard.ibus_modifiers();
        let state = if pressed { modifiers } else { modifiers | RELEASE_MASK };

        let outcome = self.process_key(keysym.raw(), evdev + XKB_KEYCODE_OFFSET, state);
        let handled = outcome.as_ref().is_some_and(|outcome| outcome.handled);
        if let Some(outcome) = outcome {
            self.apply_records(outcome.records);
        }

        // Read after the call, not before: a transport failure inside it is
        // exactly how the context disappears, and the key that discovered that
        // should be routed as if there had never been one.
        let route = router::route(KeyFacts {
            engine   : self.link.context().is_some(),
            pressed  : pressed,
            handled  : handled,
            modifiers: modifiers,
            keysym   : keysym,
            character: character,
        });
        tracing::info!(
            "key {evdev} {} keysym={} state={} handled={handled} -> {}",
            if pressed { "press" } else { "release" },
            xkbcommon::xkb::keysym_get_name(keysym),
            describe_state(state),
            match &route {
                Route::Swallow          => "swallow".to_string(),
                Route::Commit(character) => format!("commit {character:?}"),
                Route::Replay           => "replay".to_string(),
            },
        );

        match route {
            Route::Swallow           => {}
            Route::Commit(character) => self.commit_text(&character.to_string()),
            Route::Replay            => self.replay(time, evdev, pressed),
        }
    }

    /// Feeds one key to the engine, or answers `None` if there is no engine.
    ///
    /// A transport failure here takes the context down and the key falls
    /// through to the routing rules as unhandled, which is what makes "typing
    /// still works while ibus-daemon restarts" true.
    fn process_key(&mut self, keyval: u32, keycode: u32, state: u32) -> Option<crate::ibus::KeyOutcome> {
        let result = self.link.context()?.process_key(keyval, keycode, state);
        match result {
            Ok(outcome) => Some(outcome),
            Err(e) if Link::is_fatal(&e) => {
                self.link.lost(&e.to_string());
                None
            }
            Err(e) => {
                tracing::warn!("ProcessKeyEvent failed: {e}");
                None
            }
        }
    }

    /// Starts the repeat clock for a key that was just pressed.
    fn schedule_repeat(&mut self, evdev: u32) {
        let Some(delay) = self.keyboard.repeat_info().delay() else {
            return;
        };
        if !self.keyboard.repeats(evdev) {
            return;
        }

        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.repeat = Some(Repeat {
            evdev     : evdev,
            generation: generation,
        });

        if let Err(e) = self.handle.insert_source(
            Timer::from_duration(delay),
            move |_, _, frontend| frontend.on_repeat(generation),
        ) {
            tracing::warn!("could not schedule key repeat: {e}");
            self.repeat = None;
        }
    }

    /// One tick of the repeat clock.
    fn on_repeat(&mut self, generation: u64) -> TimeoutAction {
        // A timer cannot remove itself, so a cancelled repeat is one whose
        // generation no longer matches.
        let Some(repeat) = self.repeat.as_ref() else {
            return TimeoutAction::Drop;
        };
        if repeat.generation != generation {
            return TimeoutAction::Drop;
        }
        let evdev = repeat.evdev;

        let time = self.synthetic_time();
        self.deliver_key(time, evdev, true);

        // Delivering the key may have deactivated us or lost the daemon.
        if self.repeat.as_ref().is_none_or(|repeat| repeat.generation != generation) {
            return TimeoutAction::Drop;
        }

        match self.keyboard.repeat_info().period() {
            Some(period) => TimeoutAction::ToDuration(period),
            None         => TimeoutAction::Drop,
        }
    }

    /// Stops any repeat in progress.
    fn cancel_repeat(&mut self) {
        self.repeat = None;
    }

    /// A timestamp for a key we synthesised, on the compositor's clock.
    ///
    /// Extrapolated from the last real key event rather than from a clock of
    /// our own: the protocol says the base is undefined, so a client that
    /// compares timestamps — for double-click or repeat detection of its own —
    /// must not see two different bases interleaved.
    fn synthetic_time(&self) -> u32 {
        let (time, at) = self.last_key;

        time.wrapping_add(at.elapsed().as_millis() as u32)
    }
}

// --- Effects ---

impl Frontend {
    /// Applies the effects the daemon withheld while processing a key.
    fn apply_records(&mut self, records: Vec<PostRecord>) {
        for record in records {
            tracing::info!("  drain {record}");
            match record {
                PostRecord::Commit(text) => self.commit_text(&text.text),
                PostRecord::UpdatePreedit { text, cursor, visible, mode } => {
                    self.set_preedit(&text.text, cursor, visible, mode);
                }
                PostRecord::DeleteSurrounding { offset, nchars } => {
                    self.delete_surrounding(offset, nchars);
                }
                PostRecord::ForwardKey { keycode, state, .. } => {
                    self.forward_key(keycode, state);
                }
                PostRecord::HidePreedit => self.set_preedit("", 0, false, None),
                // The preedit we hold is already on screen: `set_preedit_string`
                // is the only way to show or hide one, so showing an existing
                // preedit is a no-op and hiding it clears it, which loses the
                // text a later show would want back. No engine does that.
                PostRecord::ShowPreedit => {}
                PostRecord::RequireSurrounding => {
                    self.wants_around = true;
                    self.push_surrounding();
                }
            }
        }
    }

    /// Handles a signal that arrived outside any key's drain.
    ///
    /// The mapping is the same as the drain's, because it is the same set of
    /// effects — the daemon only withholds the ones a key caused. Engines
    /// produce the rest on their own schedule: a conversion finishing, a
    /// candidate list appearing.
    fn on_upstream(&mut self, upstream: Upstream) {
        let signal = match upstream {
            Upstream::Signal(signal) => signal,
            Upstream::Lost           => {
                self.link.lost("the signal stream ended");
                return;
            }
        };

        tracing::info!("  signal {signal}");
        match signal {
            ContextSignal::CommitText(text) => self.commit_text(&text.text),
            ContextSignal::UpdatePreedit { text, cursor, visible, mode } => {
                self.set_preedit(&text.text, cursor, visible, mode);
            }
            ContextSignal::DeleteSurroundingText { offset, nchars } => {
                self.delete_surrounding(offset, nchars);
            }
            ContextSignal::ForwardKeyEvent { keycode, state, .. } => {
                self.forward_key(keycode, state);
            }
            ContextSignal::HidePreedit         => self.set_preedit("", 0, false, None),
            ContextSignal::RequireSurroundingText => {
                self.wants_around = true;
                self.push_surrounding();
            }
            // Candidates, auxiliary text and engine properties. The candidate
            // window takes what it recognises and drops the rest; engine
            // properties are phase 4's.
            other => {
                if let Some(popup) = self.popup.as_mut() {
                    popup.on_signal(other);
                }
            }
        }
    }

    /// Sends text to the application as a commit.
    fn commit_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        self.im.commit_string(text.to_string());
        self.im.commit(self.serial);
        // A commit with no preedit beside it clears the preedit, by the
        // protocol's double-buffering rules, so our record of it must go too.
        self.preedit = None;
    }

    /// Replaces the preedit.
    ///
    /// `cursor` is a character offset into `text`, because IBus counts
    /// characters; `set_preedit_string` counts bytes.
    fn set_preedit(&mut self, text: &str, cursor: u32, visible: bool, mode: Option<u32>) {
        let shown = if visible { text } else { "" };
        let caret = byte_offset(shown, cursor) as i32;

        self.im.set_preedit_string(shown.to_string(), caret, caret);
        self.im.commit(self.serial);

        self.preedit = if shown.is_empty() {
            None
        } else {
            Some(Preedit {
                text: shown.to_string(),
                mode: mode,
            })
        };
    }

    /// Commits a preedit the engine asked us to keep — if we still can.
    ///
    /// This is the point of `ClientCommitPreedit`: with it set the daemon
    /// stops committing the preedit itself and hands the decision to the
    /// client, so a half-finished mozc conversion can be finished rather than
    /// discarded. Phase 5 needs it, because entering dictation resets the
    /// context and the user's half-typed conversion should survive that.
    ///
    /// **The `active` guard is the whole subtlety, and it is not defensive
    /// programming.** `commit_string` is applied to whichever text input is
    /// active when the *compositor* processes it — smithay's handler is
    /// `with_active_text_input(|ti, _| ti.commit_string(…))`
    /// (`input_method_handle.rs:207-211`) — not to the one that was focused
    /// when we sent it. Committing after a `deactivate` therefore does not put
    /// the text back where it came from: it types it into the next window to
    /// take focus. Measured, not assumed — a pending 「こ」 landed in the
    /// window that focus moved *to*.
    ///
    /// So the honest answer for a focus change is that text-input-v3 cannot
    /// express what IBus is asking for, and the preedit is lost exactly as it
    /// is with IBus's own bridge. Losing it is bad; leaking it into another
    /// application is worse.
    fn flush_held_preedit(&mut self) {
        let Some(preedit) = self.preedit.take() else {
            return;
        };
        if !self.active {
            tracing::debug!("dropping held preedit {:?}: no field to commit it to", preedit.text);
            return;
        }
        if preedit.mode != Some(PREEDIT_COMMIT) || preedit.text.is_empty() {
            return;
        }

        tracing::info!("committing held preedit {:?}", preedit.text);
        self.im.commit_string(preedit.text);
        self.im.commit(self.serial);
    }

    /// Deletes text around the caret on the engine's behalf.
    fn delete_surrounding(&mut self, offset: i32, nchars: u32) {
        let Some(surrounding) = self.surrounding.as_ref() else {
            tracing::warn!("delete-surrounding with no surrounding text cached; ignoring");
            return;
        };

        let (before, after) = delete_range(&surrounding.text, surrounding.cursor, offset, nchars);
        if before == 0 && after == 0 {
            return;
        }

        self.im.delete_surrounding_text(before, after);
        self.im.commit(self.serial);
    }

    /// Replays a key the engine declined to consume.
    fn forward_key(&mut self, keycode: u32, state: u32) {
        if keycode < XKB_KEYCODE_OFFSET {
            // The engine forwarded a keysym with no hardware code behind it.
            // A virtual keyboard can only send codes, so this would need a
            // synthesised keymap of its own; IBus's own bridge leaves the same
            // case as a TODO on this protocol version.
            tracing::warn!("cannot forward a key with no keycode (state {state:#x})");
            return;
        }

        let time = self.synthetic_time();
        self.replay(time, keycode - XKB_KEYCODE_OFFSET, state & RELEASE_MASK == 0);
    }

    /// Sends a key through the virtual keyboard.
    ///
    /// Safe against feedback: the compositor delivers virtual-keyboard keys
    /// straight to the focused surface, bypassing both the shortcut filter and
    /// the input-method grab, so a key replayed here cannot come back to us.
    fn replay(&mut self, time: u32, evdev: u32, pressed: bool) {
        let Some(session) = self.session.as_ref() else {
            tracing::debug!("dropping key {evdev}: no virtual keyboard");
            return;
        };
        if !session.has_keymap {
            tracing::debug!("dropping key {evdev}: the virtual keyboard has no keymap yet");
            return;
        }

        session.vkbd.key(time, evdev, u32::from(pressed));
    }

    /// Tells the engine what kind of field has focus.
    fn push_content_type(&mut self) {
        let content = self.content;
        self.with_context("ContentType", |context| {
            context.set_content_type(content.purpose, content.hints)
        });
    }

    /// Hands the engine the text around the caret, converted to characters.
    fn push_surrounding(&mut self) {
        let (text, cursor, anchor) = match self.surrounding.as_ref() {
            Some(surrounding) => (
                surrounding.text.clone(),
                char_offset(&surrounding.text, surrounding.cursor),
                char_offset(&surrounding.text, surrounding.anchor),
            ),
            // An engine that asked deserves an answer even when the client
            // never sent one, or it will keep asking.
            None => (String::new(), 0, 0),
        };

        tracing::debug!(
            "surrounding text {text:?} cursor={cursor} anchor={anchor} cause={}",
            change_cause_name(self.change_cause),
        );
        self.with_context("SetSurroundingText", |context| {
            context.set_surrounding_text(&Text::plain(text), cursor, anchor)
        });
    }

    /// Runs one call against the context, if there is one, and deals with a
    /// dead connection.
    ///
    /// The closure form exists so the borrow of `self.link` ends before the
    /// error handling needs it mutably, which is also why the result is
    /// matched rather than handled inside.
    fn with_context<F>(&mut self, what: &'static str, call: F)
    where
        F: FnOnce(&crate::ibus::Context) -> crate::ibus::Result<()>,
    {
        let result = match self.link.context() {
            Some(context) => call(context),
            None          => return,
        };

        if let Err(e) = result {
            if Link::is_fatal(&e) {
                self.link.lost(&e.to_string());
            } else {
                tracing::warn!("ibus {what} failed: {e}");
            }
        }
    }
}

// --- Offset conversion ---

/// Names a `text_change_cause` for logs.
///
/// The two values are text-input-v3's (`input_method` 0, `other` 1,
/// text-input-unstable-v3.xml:173-174); an unknown one is reported rather than
/// rejected, because it can only mean a protocol version we do not know about.
fn change_cause_name(cause: u32) -> &'static str {
    match cause {
        0 => "input-method",
        1 => "other",
        _ => "unknown",
    }
}

/// Byte offset of the `chars`th character of `text`.
///
/// Clamped to the end rather than panicking: the offset comes from an engine
/// over D-Bus and a preedit that is one character shorter than the engine
/// thinks is not worth crashing the input method over.
fn byte_offset(text: &str, chars: u32) -> u32 {
    text.char_indices()
        .nth(chars as usize)
        .map_or(text.len(), |(index, _)| index) as u32
}

/// Character offset of a byte offset into `text`.
///
/// A byte offset that is not on a character boundary is rounded down, which is
/// the only reading that cannot produce an offset past the end.
fn char_offset(text: &str, bytes: u32) -> u32 {
    let limit = (bytes as usize).min(text.len());

    text[..limit.min(floor_boundary(text, limit))].chars().count() as u32
}

/// The largest character boundary at or below `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }

    index
}

/// Converts IBus's delete-surrounding request into text-input-v3's.
///
/// The two disagree about what can be expressed. IBus names a character range
/// relative to the caret and the range need not contain the caret; text-input
/// names a byte count before it and a byte count after it, so the range it
/// deletes always does. The range is therefore extended to reach the caret,
/// which over-deletes rather than under-deletes — an engine that asked for
/// text near the caret to go always gets at least what it asked for, and
/// engines in practice only ask for ranges that touch the caret anyway.
///
/// IBus's own bridge clamps differently, with `offset = MIN(offset, 0)`
/// (`ibuswaylandim.c:752`), which turns a forward-only range into an
/// unsigned underflow on the `after` side.
fn delete_range(text: &str, cursor: u32, offset: i32, nchars: u32) -> (u32, u32) {
    let caret = char_offset(text, cursor) as i64;
    let total = text.chars().count() as i64;

    let start = (caret + offset as i64).clamp(0, total);
    let end = (start + nchars as i64).clamp(0, total);

    let before = caret - start.min(caret);
    let after = end.max(caret) - caret;

    let before_bytes = byte_offset(text, caret as u32) - byte_offset(text, (caret - before) as u32);
    let after_bytes = byte_offset(text, (caret + after) as u32) - byte_offset(text, caret as u32);

    (before_bytes, after_bytes)
}

// --- Wayland dispatch ---

impl Dispatch<ZwpInputMethodV2, ()> for Frontend {
    fn event(
        frontend: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_input_method_v2::Event;

        match event {
            // The initial state of the pending events is reset here, per
            // input-method-unstable-v2.xml:73-96, so that a field which sends
            // no content type gets the defaults rather than the last field's.
            Event::Activate   => frontend.pending = Pending { active: true, ..Pending::default() },
            Event::Deactivate => frontend.pending.active = false,
            Event::SurroundingText { text, cursor, anchor } => {
                frontend.pending.surrounding = Some(Surrounding {
                    text  : text,
                    cursor: cursor,
                    anchor: anchor,
                });
            }
            Event::TextChangeCause { cause } => {
                frontend.pending.change_cause = into_raw(cause);
            }
            Event::ContentType { hint, purpose } => {
                frontend.pending.content =
                    ContentType::from_wayland(into_raw(hint), into_raw(purpose));
            }
            Event::Done => frontend.on_done(),
            Event::Unavailable => {
                tracing::error!(
                    "the compositor took the input method away; another client bound it"
                );
                frontend.stop.stop();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for Frontend {
    fn event(
        frontend: &mut Self,
        _: &ZwpInputMethodKeyboardGrabV2,
        event: zwp_input_method_keyboard_grab_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_input_method_keyboard_grab_v2::Event;

        match event {
            Event::Keymap { format, fd, size } => {
                // wlroots rejects a zero-sized keymap with a protocol error
                // and smithay would mmap nothing, so it is never forwarded.
                if size == 0 {
                    tracing::warn!("ignoring a zero-sized keymap");
                    return;
                }

                // Forwarded before it is parsed, so that replaying keys keeps
                // working even if the keymap is one xkbcommon cannot compile.
                if let Some(session) = frontend.session.as_mut() {
                    session.vkbd.keymap(into_raw(format), fd.as_fd(), size);
                    session.has_keymap = true;
                }

                if let Err(e) = frontend.keyboard.load_keymap(&fd, size) {
                    tracing::error!("could not use the compositor's keymap: {e:#}");
                }
            }
            Event::Key { time, key, state, .. } => {
                let pressed = into_raw(state) == u32::from(wl_keyboard::KeyState::Pressed);
                frontend.on_key(time, key, pressed);
            }
            Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                frontend
                    .keyboard
                    .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
                // Forwarded raw, not re-derived: the virtual keyboard's
                // consumer is the same compositor that sent them, and it
                // expects its own numbering back.
                if let Some(session) = frontend.session.as_ref()
                    && session.has_keymap
                {
                    session
                        .vkbd
                        .modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            }
            Event::RepeatInfo { rate, delay } => {
                tracing::debug!("repeat rate {rate}/s after {delay}ms");
                frontend.keyboard.set_repeat_info(RepeatInfo {
                    rate : rate,
                    delay: delay,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Frontend {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(Frontend: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(Frontend: ignore ZwpInputMethodManagerV2);
wayland_client::delegate_noop!(Frontend: ignore ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(Frontend: ignore ZwpVirtualKeyboardV1);

/// Unwraps a protocol enum back to the number on the wire.
///
/// The enums are relayed, not interpreted — a keymap format goes straight on to
/// the virtual keyboard, a key state is compared against one value — so an
/// unknown variant must survive rather than being rejected.
fn into_raw<T>(value: WEnum<T>) -> u32
where
    u32: From<T>,
{
    match value {
        WEnum::Value(value)   => u32::from(value),
        WEnum::Unknown(value) => value,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversions IBus and Wayland disagree about. Everything IBus says
    /// is in characters; everything Wayland says is in bytes; getting it wrong
    /// is invisible in ASCII and corrupts every CJK preedit.
    #[test]
    fn converts_offsets_across_the_boundary() {
        assert_eq!(byte_offset("にほん", 0), 0);
        assert_eq!(byte_offset("にほん", 1), 3);
        assert_eq!(byte_offset("にほん", 3), 9);
        assert_eq!(char_offset("にほん", 9), 3);
        assert_eq!(char_offset("にほん", 3), 1);
    }

    /// An offset past the end, or one landing mid-character, has to answer
    /// something sane: these come off the wire from an engine.
    #[test]
    fn clamps_offsets_off_the_end() {
        assert_eq!(byte_offset("abc", 99), 3);
        assert_eq!(char_offset("abc", 99), 3);
        // Byte 1 is inside the first character of にほん.
        assert_eq!(char_offset("にほん", 1), 0);
    }

    /// The common case: an engine replacing the word behind the caret.
    #[test]
    fn deletes_the_characters_behind_the_caret() {
        // "hello|" with the caret at byte 5, delete the last two characters.
        assert_eq!(delete_range("hello", 5, -2, 2), (2, 0));
    }

    /// Character counts become byte counts, which for CJK is a factor of three.
    #[test]
    fn counts_bytes_not_characters() {
        // "にほん|", delete the two characters before the caret.
        assert_eq!(delete_range("にほん", 9, -2, 2), (6, 0));
    }

    /// A range that straddles the caret becomes a count on each side.
    #[test]
    fn splits_a_range_around_the_caret() {
        // "ab|cd", delete one character either side.
        assert_eq!(delete_range("abcd", 2, -1, 2), (1, 1));
    }

    /// A range that does not reach the caret cannot be expressed, so it is
    /// extended until it does. Under-deleting would leave the engine and the
    /// application disagreeing about the text, which is worse.
    #[test]
    fn extends_a_detached_range_to_the_caret() {
        // "abcdef|", asking for characters 1..3 with the caret at 6.
        assert_eq!(delete_range("abcdef", 6, -5, 2), (5, 0));
        // A forward-only range, which is where upstream's clamp underflows.
        assert_eq!(delete_range("abcdef", 2, 2, 2), (0, 4));
    }

    /// Nothing to delete has to stay nothing, or every no-op request sends a
    /// spurious commit.
    #[test]
    fn deletes_nothing_when_asked_for_nothing() {
        assert_eq!(delete_range("abc", 1, 0, 0), (0, 0));
    }
}
