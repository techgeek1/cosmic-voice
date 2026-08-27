//! An input context: the per-focus object that keys go into and text comes
//! out of.
//!
//! # The synchronous key path
//!
//! IBus grew up asynchronous, and the asynchronous contract has a race that
//! cannot be fixed by the client: `ProcessKeyEvent` answers "handled" or "not
//! handled", but a handled key's *effects* (a commit, a preedit update, a key
//! the engine wants forwarded) arrive as separate signals which may overtake
//! the reply. A client that passes unhandled keys through to the application
//! therefore sometimes types a character after the engine already committed
//! one.
//!
//! Since 1.5.28 there is a synchronous mode, and since 1.5.29 it is what GTK
//! uses by default. Setting `EffectivePostProcessKeyEvent` to true makes the
//! daemon *withhold* those effects for the duration of a `ProcessKeyEvent`
//! call: instead of emitting signals it queues records
//! (`bus/inputcontext.c:1838-1900`, `bus_input_context_make_post_process_key_event`),
//! and the client reads the queue back from the `PostProcessKeyEvent` property
//! once the call returns. Reply and effects then arrive in one atomic step, in
//! order, and the race is gone by construction.
//!
//! Only effects caused *by that key* are withheld. Everything an engine does
//! on its own — a lookup table appearing because a conversion finished, an
//! auxiliary-text update, a property change — still arrives as an ordinary
//! signal, which is why [`Context::next_signal`] exists alongside the drain
//! and must keep being polled.
//!
//! # The drain's record encoding
//!
//! `PostProcessKeyEvent` is an `a(yv)`: a one-byte tag and an `IBusText`.
//! There is no other envelope, so the daemon formats non-text payloads into
//! the text field with `ibus_text_new_from_printf()`. From
//! `bus/inputcontext.c:1849-1895`:
//!
//! ```text
//! 'c'  commit                 text is the IBusText to commit
//! 'd'  delete-surrounding     text is printf("%d,%u", offset, nchars)
//! 'f'  forward-key-event      text is printf("%u,%u,%u", keyval, keycode, state)
//! 'h'  hide-preedit           text is ""
//! 'r'  require-surrounding    text is ""
//! 's'  show-preedit           text is ""
//! 'u'  update-preedit         TWO records: the IBusText, then
//!                             printf("%u,%u", cursor_pos, visible)
//! 'm'  update-preedit-with-mode  TWO records: the IBusText, then
//!                             printf("%u,%u,%u", cursor_pos, visible, mode)
//! ```
//!
//! `'m'` replaces `'u'` exactly when `ClientCommitPreedit` is set
//! (`bus/inputcontext.c:3505-3507`), which we always set, so `'u'` should not
//! appear in practice — both are decoded anyway. The reference decoder is
//! `ibus_input_context_post_process_key_event()` in `src/ibusinputcontext.c`,
//! lines 1599-1712, and the field parsers immediately above it.
//!
//! The queue is capped at 30 records (`MAX_SYNC_DATA`, `bus/inputcontext.c:34`)
//! and the daemon warns and drops beyond that.

use std::time::{Duration, Instant};

use zbus::zvariant::{Array, OwnedObjectPath, OwnedValue, Value};

use super::text::{EngineDesc, LookupTable, PropList, Property, Serializable, Text};
use super::{Error, Result, Signals};

// --- Capability bits (ibustypes.h:119-127) ---

/// The client renders preedit itself.
pub const CAP_PREEDIT_TEXT: u32 = 1 << 0;
/// The client renders the engine's auxiliary text.
pub const CAP_AUXILIARY_TEXT: u32 = 1 << 1;
/// The client renders the candidate table.
pub const CAP_LOOKUP_TABLE: u32 = 1 << 2;
/// The client takes and gives up focus explicitly.
pub const CAP_FOCUS: u32 = 1 << 3;
/// The client renders engine properties (the status menu).
pub const CAP_PROPERTY: u32 = 1 << 4;
/// The client can supply the text around the cursor.
pub const CAP_SURROUNDING_TEXT: u32 = 1 << 5;
/// The client is an on-screen keyboard.
pub const CAP_OSK: u32 = 1 << 6;
/// The client uses the synchronous key path. Advisory: the behaviour is
/// actually driven by the `EffectivePostProcessKeyEvent` property, and the
/// daemon does not consult this bit on the context path.
pub const CAP_SYNC_PROCESS_KEY: u32 = 1 << 7;

/// What the multiplexer asks for.
///
/// The set differs from the one IBus's own Wayland bridge uses, and the
/// difference is the point: `LOOKUP_TABLE` and `AUXILIARY_TEXT` route
/// candidates to *us* rather than to a panel process
/// (`bus/inputcontext.c:2334-2341`), and there is no panel process any more.
/// `PROPERTY` is in for the same reason: with it, `RegisterProperties` and
/// `UpdateProperty` are emitted as signals on this context
/// (`bus/inputcontext.c:2544-2551`, `:2573-2580`) and `PropertyActivate` goes
/// straight to the engine (`:1375-1387`); without it the engine's status menu
/// goes to the panel service, and the only panel service there ever was is
/// the one the cutover retired. The applet popup is the menu now (phase 6).
pub const CAPABILITIES: u32 = CAP_PREEDIT_TEXT
    | CAP_AUXILIARY_TEXT
    | CAP_LOOKUP_TABLE
    | CAP_FOCUS
    | CAP_PROPERTY
    | CAP_SURROUNDING_TEXT;

// --- Modifier bits (ibustypes.h:70-99) ---

/// Shift is down.
pub const SHIFT_MASK: u32 = 1 << 0;
/// Caps Lock is on.
pub const LOCK_MASK: u32 = 1 << 1;
/// Control is down.
pub const CONTROL_MASK: u32 = 1 << 2;
/// Mod1, conventionally Alt.
pub const MOD1_MASK: u32 = 1 << 3;
/// Mod2, conventionally Num Lock.
pub const MOD2_MASK: u32 = 1 << 4;
/// Mod3.
pub const MOD3_MASK: u32 = 1 << 5;
/// Mod4, conventionally Super.
pub const MOD4_MASK: u32 = 1 << 6;
/// Mod5, conventionally ISO_Level3_Shift.
pub const MOD5_MASK: u32 = 1 << 7;
/// Mouse button 1.
pub const BUTTON1_MASK: u32 = 1 << 8;
/// Mouse button 2.
pub const BUTTON2_MASK: u32 = 1 << 9;
/// Mouse button 3.
pub const BUTTON3_MASK: u32 = 1 << 10;
/// Mouse button 4.
pub const BUTTON4_MASK: u32 = 1 << 11;
/// Mouse button 5.
pub const BUTTON5_MASK: u32 = 1 << 12;
/// The event was handled by IBus. Set on events IBus hands back.
pub const HANDLED_MASK: u32 = 1 << 24;
/// The event was forwarded *from* IBus, so a client must not feed it back in.
/// `IBUS_IGNORED_MASK` is an alias of this same bit (ibustypes.h:91), not a
/// separate flag.
pub const FORWARD_MASK: u32 = 1 << 25;
/// Super (Windows key).
pub const SUPER_MASK: u32 = 1 << 26;
/// Hyper.
pub const HYPER_MASK: u32 = 1 << 27;
/// Meta.
pub const META_MASK: u32 = 1 << 28;
/// The key is being released. Bit 30, not 29 — bit 29 is unused.
pub const RELEASE_MASK: u32 = 1 << 30;
/// Every bit above that is a modifier rather than a marker.
pub const MODIFIER_MASK: u32 = 0x5f00_1fff;
/// The modifiers upstream considers capable of turning a key into a shortcut
/// (`IBUS_MODIFIER_FILTER`, `ibustypes.h:387-398`): everything in
/// [`MODIFIER_MASK`] except the two locks, the mouse buttons, and the three
/// high-level aliases Super/Hyper/Meta that duplicate a Mod bit.
///
/// The frontend's "is this a plain printable keystroke" test is this set minus
/// Shift. Deriving it from upstream rather than writing the bits out is what
/// keeps Caps Lock and Num Lock from silently disabling the commit-as-text
/// path, which is what happens if the test is `MODIFIER_MASK & !SHIFT_MASK`.
pub const MODIFIER_FILTER: u32 = MODIFIER_MASK
    & !(LOCK_MASK
        | MOD2_MASK
        | BUTTON1_MASK
        | BUTTON2_MASK
        | BUTTON3_MASK
        | BUTTON4_MASK
        | BUTTON5_MASK
        | SUPER_MASK
        | HYPER_MASK
        | META_MASK);

/// Renders a capability set for logs.
pub fn describe_capabilities(caps: u32) -> String {
    let mut names = Vec::new();
    for (bit, name) in [
        (CAP_PREEDIT_TEXT, "preedit"),
        (CAP_AUXILIARY_TEXT, "auxiliary"),
        (CAP_LOOKUP_TABLE, "lookup-table"),
        (CAP_FOCUS, "focus"),
        (CAP_PROPERTY, "property"),
        (CAP_SURROUNDING_TEXT, "surrounding-text"),
        (CAP_OSK, "osk"),
        (CAP_SYNC_PROCESS_KEY, "sync-process-key"),
    ] {
        if caps & bit != 0 {
            names.push(name);
        }
    }

    if names.is_empty() { "none".to_string() } else { names.join("|") }
}

/// Renders a key-event state word for logs.
///
/// Worth having because the failure mode when the state word is wrong is not
/// an error but an engine that quietly ignores a key, and reading a hex mask
/// by eye is how that gets missed.
pub fn describe_state(state: u32) -> String {
    let mut names = Vec::new();
    for (bit, name) in [
        (SHIFT_MASK, "shift"),
        (LOCK_MASK, "lock"),
        (CONTROL_MASK, "control"),
        (MOD1_MASK, "mod1"),
        (MOD2_MASK, "mod2"),
        (MOD3_MASK, "mod3"),
        (MOD4_MASK, "mod4"),
        (MOD5_MASK, "mod5"),
        (BUTTON1_MASK, "button1"),
        (BUTTON2_MASK, "button2"),
        (BUTTON3_MASK, "button3"),
        (BUTTON4_MASK, "button4"),
        (BUTTON5_MASK, "button5"),
        (HANDLED_MASK, "handled"),
        (FORWARD_MASK, "forward"),
        (SUPER_MASK, "super"),
        (HYPER_MASK, "hyper"),
        (META_MASK, "meta"),
        (RELEASE_MASK, "release"),
    ] {
        if state & bit != 0 {
            names.push(name.to_string());
        }
    }

    let unknown = state & !MODIFIER_MASK & !HANDLED_MASK & !FORWARD_MASK & !RELEASE_MASK;
    if unknown != 0 {
        names.push(format!("unknown:{unknown:#x}"));
    }

    if names.is_empty() { "none".to_string() } else { names.join("|") }
}

// --- Preedit commit modes (ibustypes.h:137-140) ---

/// On focus loss the preedit is discarded.
pub const PREEDIT_CLEAR: u32 = 0;
/// On focus loss the preedit is committed. With `ClientCommitPreedit` set, the
/// *client* has to do that committing — which is the data-loss bug IBus's own
/// Wayland bridge has and the multiplexer fixes.
pub const PREEDIT_COMMIT: u32 = 1;

// --- Proxies ---

/// `org.freedesktop.IBus.InputContext`, bound against ibus 1.5.34's
/// `introspection_xml` in `bus/inputcontext.c:255-368`.
///
/// The interface's signals are deliberately *not* declared here. They are
/// decoded from raw messages into [`ContextSignal`] instead, so that one
/// stream and one `match` cover all twenty of them and the phase-2 calloop
/// thread has a single pollable source rather than twenty.
///
/// Note that upstream's introspection XML declares `ClientCommitPreedit`,
/// `ContentType` and `EffectivePostProcessKeyEvent` as write-only while live
/// introspection reports them readable; the daemon implements no getters for
/// them (`bus/inputcontext.c:1677` lists `PostProcessKeyEvent` as the single
/// readable property), so only setters are bound.
#[zbus::proxy(
    interface       = "org.freedesktop.IBus.InputContext",
    default_service = "org.freedesktop.IBus",
    gen_async       = false
)]
pub trait InputContext {
    /// Takes IBus focus. The engine starts seeing our keys, and stops seeing
    /// anyone else's.
    fn focus_in(&self) -> zbus::Result<()>;

    /// Gives IBus focus up.
    fn focus_out(&self) -> zbus::Result<()>;

    /// Drops any conversion in progress.
    fn reset(&self) -> zbus::Result<()>;

    /// Switches this context to a named engine.
    fn set_engine(&self, name: &str) -> zbus::Result<()>;

    /// The engine currently attached, as an `IBusEngineDesc`.
    fn get_engine(&self) -> zbus::Result<OwnedValue>;

    /// The key path. `keyval` is an X keysym, `keycode` is evdev+8, and
    /// `state` is the modifier bits above with [`RELEASE_MASK`] on key-up.
    /// Returns whether the engine consumed the key.
    fn process_key_event(&self, keyval: u32, keycode: u32, state: u32) -> zbus::Result<bool>;

    /// Declares what this client can render. Must be called before `FocusIn`;
    /// the daemon uses it to decide whether to talk to us or to a panel.
    fn set_capabilities(&self, caps: u32) -> zbus::Result<()>;

    /// Where the caret is, in screen coordinates, for positioning the
    /// candidate window. Under Wayland we have no screen coordinates, so this
    /// is a formality — cosmic-comp positions our popup surface itself.
    fn set_cursor_location(&self, x: i32, y: i32, w: i32, h: i32) -> zbus::Result<()>;

    /// Hands the engine the text around the cursor, as an `IBusText` plus
    /// character offsets. Engines ask for it with `RequireSurroundingText`.
    fn set_surrounding_text(
        &self,
        text      : &Value<'_>,
        cursor_pos: u32,
        anchor_pos: u32,
    ) -> zbus::Result<()>;

    /// The same as `SetCursorLocation` but relative to the last one, which
    /// saves a round trip when the caret moves within a line. Only useful once
    /// there is a caret to track, which is phase 3.
    fn set_cursor_location_relative(&self, x: i32, y: i32, w: i32, h: i32) -> zbus::Result<()>;

    /// Activates one of the engine's properties — the status-menu entries an
    /// engine registers. Dispatched to this context's engine
    /// (`bus/inputcontext.c:1375-1387`), so it needs no focus.
    fn property_activate(&self, name: &str, state: u32) -> zbus::Result<()>;

    /// Feeds handwriting stroke coordinates to the engine. Bound for interface
    /// completeness; nothing in this project produces strokes.
    fn process_hand_writing_event(&self, coordinates: &[f64]) -> zbus::Result<()>;

    /// Cancels the last `n_strokes` handwriting strokes.
    fn cancel_hand_writing(&self, n_strokes: u32) -> zbus::Result<()>;

    /// Declares what kind of field has focus, as `(purpose, hints)`. Phase 2
    /// relays it from `text-input-v3`'s content type; the mapping tables are in
    /// IBus's own bridge at `client/wayland/ibuswaylandim.c:2230-2323`.
    #[zbus(property(emits_changed_signal = "false"))]
    fn set_content_type(&self, value: (u32, u32)) -> zbus::Result<()>;

    /// Makes the *client* responsible for committing preedit when the engine
    /// says so, rather than the daemon doing it behind our back. Required for
    /// the multiplexer's turn-taking: entering dictation has to commit a
    /// half-finished conversion, which is only possible if we hold it.
    #[zbus(property(emits_changed_signal = "false"))]
    fn set_client_commit_preedit(&self, value: (bool,)) -> zbus::Result<()>;

    /// Switches on the synchronous key path described at the top of this
    /// module.
    #[zbus(property(emits_changed_signal = "false"))]
    fn set_effective_post_process_key_event(&self, value: (bool,)) -> zbus::Result<()>;

    /// Drains the effects withheld during the last `ProcessKeyEvent`.
    ///
    /// Declared uncached on purpose. Reading it *is* the drain — it pops the
    /// daemon's queue — so a cached read would return an empty list forever
    /// and a `GetAll` would drain it at a moment of zbus's choosing.
    #[zbus(property(emits_changed_signal = "false"))]
    fn post_process_key_event(&self) -> zbus::Result<OwnedValue>;
}

/// `org.freedesktop.IBus.Service`, present on every object the daemon exports.
#[zbus::proxy(
    interface       = "org.freedesktop.IBus.Service",
    default_service = "org.freedesktop.IBus",
    gen_async       = false
)]
pub trait Service {
    /// Destroys the object. The daemon does this for us when the connection
    /// drops, but doing it explicitly bounds how long a stale context can hold
    /// engine state.
    fn destroy(&self) -> zbus::Result<()>;
}

// --- The drain ---

/// One effect the daemon withheld while processing a key.
///
/// The order within a drain is the order the engine produced them, and it
/// matters: an engine that commits then updates preedit produces a different
/// result from one that does the reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostRecord {
    /// Text the engine committed. Goes to the application verbatim.
    Commit(Text),
    /// A key the engine declined and wants replayed. `state` is as the engine
    /// gave it; IBus's own client ORs [`FORWARD_MASK`] in before re-emitting,
    /// to stop the key looping back into the engine. Our frontend replays
    /// through the virtual keyboard instead, where the bit is meaningless, so
    /// it is left off here.
    ForwardKey {
        /// X keysym.
        keyval : u32,
        /// evdev+8.
        keycode: u32,
        /// Modifier bits.
        state  : u32,
    },
    /// Delete text around the cursor before committing. Offsets are in
    /// characters and `offset` is signed and relative to the cursor.
    DeleteSurrounding {
        /// Where to start, relative to the cursor, in characters.
        offset: i32,
        /// How many characters to remove.
        nchars: u32,
    },
    /// New preedit content. `mode` is present only for the `'m'` encoding and
    /// is one of [`PREEDIT_CLEAR`] / [`PREEDIT_COMMIT`].
    UpdatePreedit {
        /// The preedit text and its decoration.
        text   : Text,
        /// Cursor position within the preedit, in characters.
        cursor : u32,
        /// Whether it should be shown at all.
        visible: bool,
        /// What to do with it if focus is lost.
        mode   : Option<u32>,
    },
    /// Show the preedit that was already sent.
    ShowPreedit,
    /// Hide the preedit without clearing it.
    HidePreedit,
    /// The engine wants `SetSurroundingText` called.
    RequireSurrounding,
}

impl std::fmt::Display for PostRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostRecord::Commit(text) => write!(f, "commit {text}"),
            PostRecord::ForwardKey { keyval, keycode, state } => write!(
                f,
                "forward-key keyval={keyval:#x} keycode={keycode} state={}",
                describe_state(*state)
            ),
            PostRecord::DeleteSurrounding { offset, nchars } => {
                write!(f, "delete-surrounding offset={offset} nchars={nchars}")
            }
            PostRecord::UpdatePreedit { text, cursor, visible, mode } => write!(
                f,
                "update-preedit {text} cursor={cursor} visible={visible} mode={}",
                match mode {
                    Some(PREEDIT_CLEAR)  => "clear".to_string(),
                    Some(PREEDIT_COMMIT) => "commit".to_string(),
                    Some(other)          => format!("{other}"),
                    None                 => "-".to_string(),
                }
            ),
            PostRecord::ShowPreedit        => write!(f, "show-preedit"),
            PostRecord::HidePreedit        => write!(f, "hide-preedit"),
            PostRecord::RequireSurrounding => write!(f, "require-surrounding"),
        }
    }
}

/// What one key produced.
#[derive(Debug, Clone)]
pub struct KeyOutcome {
    /// Whether the engine consumed the key. When false the frontend has to
    /// deal with it: commit it as text if it is plain and printable, otherwise
    /// replay it through the virtual keyboard.
    pub handled: bool,
    /// The effects the daemon withheld during the call, in order. Non-empty
    /// with `handled == false` is normal — an engine can commit a pending
    /// conversion and still decline the key that ended it.
    pub records: Vec<PostRecord>,
}

// --- Signals ---

/// An asynchronous update from the engine, outside any key's drain.
#[derive(Debug, Clone)]
pub enum ContextSignal {
    /// Text to commit.
    CommitText(Text),
    /// A key to replay.
    ForwardKeyEvent {
        /// X keysym.
        keyval : u32,
        /// evdev+8.
        keycode: u32,
        /// Modifier bits.
        state  : u32,
    },
    /// New preedit content. `mode` is present when the signal was
    /// `UpdatePreeditTextWithMode`, which is what a client with
    /// `ClientCommitPreedit` set receives.
    UpdatePreedit {
        /// The preedit text and its decoration.
        text   : Text,
        /// Cursor position within the preedit, in characters.
        cursor : u32,
        /// Whether it should be shown.
        visible: bool,
        /// What to do with it if focus is lost.
        mode   : Option<u32>,
    },
    /// Show the current preedit.
    ShowPreedit,
    /// Hide the current preedit.
    HidePreedit,
    /// New auxiliary text — mozc uses it for the conversion-mode hint.
    UpdateAuxiliary {
        /// The text.
        text   : Text,
        /// Whether to show it.
        visible: bool,
    },
    /// Show the auxiliary text.
    ShowAuxiliary,
    /// Hide the auxiliary text.
    HideAuxiliary,
    /// New candidate table.
    UpdateLookupTable {
        /// The candidates and their state.
        table  : LookupTable,
        /// Whether to show it.
        visible: bool,
    },
    /// Show the candidate table.
    ShowLookupTable,
    /// Hide the candidate table.
    HideLookupTable,
    /// Move the candidate table one page back.
    PageUpLookupTable,
    /// Move the candidate table one page forward.
    PageDownLookupTable,
    /// Move the selection up.
    CursorUpLookupTable,
    /// Move the selection down.
    CursorDownLookupTable,
    /// Delete text around the cursor. **Not in the introspection XML** — the
    /// daemon emits it anyway (`bus/inputcontext.c:2666-2686`), because signal
    /// emission does not check against the declared interface.
    DeleteSurroundingText {
        /// Where to start, relative to the cursor, in characters.
        offset: i32,
        /// How many characters to remove.
        nchars: u32,
    },
    /// The engine wants `SetSurroundingText` called. Also absent from the
    /// introspection XML, also emitted (`bus/inputcontext.c:2693-2709`).
    RequireSurroundingText,
    /// The engine's whole status menu, replacing whatever it registered
    /// before. mozc sends it on every `FocusIn`; the daemon itself sends an
    /// *empty* one on focus-out and when the engine is unset
    /// (`bus/inputcontext.c:2037`, `:3051`), which is the only way an xkb
    /// engine's empty menu ever arrives.
    RegisterProperties(PropList),
    /// One entry changed, matched by key at any depth. mozc's input-mode
    /// switch is a burst of these: one per radio child, then the menu itself
    /// with its new symbol (`unix/ibus/property_handler.cc`,
    /// `UpdateCompositionModeIcon`). Boxed because a property is a tree of
    /// strings and every other signal is a few words; the channel this
    /// travels carries the whole enum by value.
    UpdateProperty(Box<Property>),
}

impl std::fmt::Display for ContextSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextSignal::CommitText(text) => write!(f, "CommitText {text}"),
            ContextSignal::ForwardKeyEvent { keyval, keycode, state } => write!(
                f,
                "ForwardKeyEvent keyval={keyval:#x} keycode={keycode} state={}",
                describe_state(*state)
            ),
            ContextSignal::UpdatePreedit { text, cursor, visible, mode } => write!(
                f,
                "UpdatePreedit {text} cursor={cursor} visible={visible} mode={}",
                match mode {
                    Some(PREEDIT_CLEAR)  => "clear".to_string(),
                    Some(PREEDIT_COMMIT) => "commit".to_string(),
                    Some(other)          => format!("{other}"),
                    None                 => "-".to_string(),
                }
            ),
            ContextSignal::ShowPreedit => write!(f, "ShowPreeditText"),
            ContextSignal::HidePreedit => write!(f, "HidePreeditText"),
            ContextSignal::UpdateAuxiliary { text, visible } => {
                write!(f, "UpdateAuxiliaryText {text} visible={visible}")
            }
            ContextSignal::ShowAuxiliary => write!(f, "ShowAuxiliaryText"),
            ContextSignal::HideAuxiliary => write!(f, "HideAuxiliaryText"),
            ContextSignal::UpdateLookupTable { table, visible } => {
                write!(f, "UpdateLookupTable visible={visible} {table}")
            }
            ContextSignal::ShowLookupTable       => write!(f, "ShowLookupTable"),
            ContextSignal::HideLookupTable       => write!(f, "HideLookupTable"),
            ContextSignal::PageUpLookupTable     => write!(f, "PageUpLookupTable"),
            ContextSignal::PageDownLookupTable   => write!(f, "PageDownLookupTable"),
            ContextSignal::CursorUpLookupTable   => write!(f, "CursorUpLookupTable"),
            ContextSignal::CursorDownLookupTable => write!(f, "CursorDownLookupTable"),
            ContextSignal::DeleteSurroundingText { offset, nchars } => {
                write!(f, "DeleteSurroundingText offset={offset} nchars={nchars}")
            }
            ContextSignal::RequireSurroundingText => write!(f, "RequireSurroundingText"),
            ContextSignal::RegisterProperties(props) => {
                write!(f, "RegisterProperties {props}")
            }
            ContextSignal::UpdateProperty(prop) => write!(f, "UpdateProperty {prop}"),
        }
    }
}

// --- The context ---

/// The interface signals arrive on, used to filter the message stream.
const INPUT_CONTEXT_INTERFACE: &str = "org.freedesktop.IBus.InputContext";

/// The decoded signal stream of one context, detachable from it.
///
/// It exists as a separate object because the frontend needs the two halves of
/// a context on two threads: [`Context::process_key`] is called inline from the
/// calloop loop, while something has to sit blocked in `next` so that signals
/// arrive without polling. One `&mut Context` cannot be in both places, so the
/// stream moves out with [`Context::take_signals`] and the rest of the context
/// stays behind.
///
/// The stream also *ends* when the connection dies, which is precisely the
/// signal the frontend needs to drop its context and start passing keys
/// through raw until ibus-daemon comes back.
pub struct SignalStream {
    /// The object path whose signals this keeps; everything else is skipped.
    path   : OwnedObjectPath,
    /// The raw message stream underneath.
    signals: Signals,
}

impl SignalStream {
    /// Waits for the next signal addressed to this context.
    ///
    /// `None` means the timeout expired with nothing for us, or the connection
    /// closed. Messages for other objects, and signals we do not decode, are
    /// skipped without resetting the deadline.
    pub fn next(&mut self, timeout: Option<Duration>) -> Result<Option<ContextSignal>> {
        let deadline = timeout.map(|limit| Instant::now() + limit);

        loop {
            let remaining = match deadline {
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(remaining) => Some(remaining),
                    None            => return Ok(None),
                },
                None => None,
            };

            let Some(message) = self.signals.next(remaining)? else {
                return Ok(None);
            };

            let header = message.header();
            if message.message_type() != zbus::message::Type::Signal {
                continue;
            }
            if header.path().map(|path| path.as_str()) != Some(self.path.as_str()) {
                continue;
            }
            if header.interface().map(|name| name.as_str()) != Some(INPUT_CONTEXT_INTERFACE) {
                continue;
            }
            let Some(member) = header.member() else {
                continue;
            };

            if let Some(signal) = decode_signal(member.as_str(), &message)? {
                return Ok(Some(signal));
            }
        }
    }
}

/// One input context, in the state the multiplexer needs.
///
/// Created through [`super::Bus::create_input_context`], which applies the
/// lifecycle: `ClientCommitPreedit`, then `EffectivePostProcessKeyEvent`, then
/// `SetCapabilities`, all of which must be in place *before* the first
/// `FocusIn` because the daemon reads them when it attaches an engine.
///
/// The context is reused across activations — `FocusIn` and `FocusOut` rather
/// than create and destroy — which is what GTK does and what keeps a
/// conversion alive across a focus change.
///
/// **Poll [`Context::next_signal`].** The signal stream has a bounded queue
/// and stalls the connection when it fills.
pub struct Context {
    /// The context's object path, which is also its identity in the daemon's
    /// `CurrentInputContext`.
    path   : OwnedObjectPath,
    /// Proxy for the input-context interface.
    proxy  : InputContextProxy<'static>,
    /// Proxy for the `Destroy` method, which lives on a different interface of
    /// the same object.
    service: ServiceProxy<'static>,
    /// Everything the daemon sends us that is not a method reply, until
    /// [`Context::take_signals`] moves it to a thread of its own.
    signals: Option<SignalStream>,
}

impl Context {
    /// Applies the lifecycle to a freshly created context object.
    pub(super) fn create(
        connection: &zbus::blocking::Connection,
        path      : OwnedObjectPath,
    ) -> Result<Self> {
        // Before any call that could provoke a signal, so nothing is missed
        // between creation and the first poll.
        let signals = Signals::new(connection);

        let proxy = InputContextProxy::builder(connection)
            .path(path.clone())?
            .build()?;
        let service = ServiceProxy::builder(connection)
            .path(path.clone())?
            .build()?;

        proxy.set_client_commit_preedit((true,))?;
        proxy.set_effective_post_process_key_event((true,))?;
        proxy.set_capabilities(CAPABILITIES)?;

        Ok(Self {
            path   : path.clone(),
            proxy  : proxy,
            service: service,
            signals: Some(SignalStream {
                path   : path,
                signals: signals,
            }),
        })
    }

    /// The context's object path.
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    /// Takes IBus focus.
    ///
    /// This is the call that steals the engine from whatever real window had
    /// it, so the frontend must only make it when a text-input client has
    /// actually activated us.
    pub fn focus_in(&self) -> Result<()> {
        Ok(self.proxy.focus_in()?)
    }

    /// Gives IBus focus up.
    pub fn focus_out(&self) -> Result<()> {
        Ok(self.proxy.focus_out()?)
    }

    /// Drops any conversion in progress.
    pub fn reset(&self) -> Result<()> {
        Ok(self.proxy.reset()?)
    }

    /// Switches this context to a named engine, e.g. `mozc-jp`.
    pub fn set_engine(&self, name: &str) -> Result<()> {
        Ok(self.proxy.set_engine(name)?)
    }

    /// The engine attached right now.
    pub fn engine(&self) -> Result<EngineDesc> {
        let value = self.proxy.get_engine()?;

        EngineDesc::from_value(&value)
    }

    /// Activates one of the engine's status-menu entries.
    ///
    /// `state` is what the entry should become, as a `PROP_STATE_*` value.
    /// That is not decoration: mozc acts on an input-mode radio only when the
    /// state sent is `CHECKED` (`unix/ibus/property_handler.cc`,
    /// `ProcessPropertyActivate`), and it ignores the state entirely for its
    /// tool entries. The engine answers with `UpdateProperty` signals, not
    /// with the method's reply, so success here means "delivered".
    pub fn property_activate(&self, key: &str, state: u32) -> Result<()> {
        Ok(self.proxy.property_activate(key, state)?)
    }

    /// Tells the daemon where the caret is. A formality under Wayland, but
    /// engines that have never seen a zero rectangle are better not surprised.
    pub fn set_cursor_location(&self, x: i32, y: i32, width: i32, height: i32) -> Result<()> {
        Ok(self.proxy.set_cursor_location(x, y, width, height)?)
    }

    /// Hands the engine the text around the cursor. Offsets are in characters.
    pub fn set_surrounding_text(&self, text: &Text, cursor: u32, anchor: u32) -> Result<()> {
        Ok(self
            .proxy
            .set_surrounding_text(&text.to_value()?, cursor, anchor)?)
    }

    /// Tells the engine what kind of field has focus.
    ///
    /// Relayed from `text-input-v3`'s content type through the translation in
    /// [`crate::im::content_type`]; engines act on it, mozc most visibly by
    /// turning itself off in password fields.
    pub fn set_content_type(&self, purpose: u32, hints: u32) -> Result<()> {
        Ok(self.proxy.set_content_type((purpose, hints))?)
    }

    /// Feeds one key through the engine and returns everything that came of
    /// it, synchronously. See the module documentation for why this blocks and
    /// why that is the correct design rather than a compromise.
    pub fn process_key(&self, keyval: u32, keycode: u32, state: u32) -> Result<KeyOutcome> {
        let handled = self.proxy.process_key_event(keyval, keycode, state)?;

        Ok(KeyOutcome {
            handled: handled,
            records: self.drain()?,
        })
    }

    /// Reads and decodes the withheld-effects queue.
    fn drain(&self) -> Result<Vec<PostRecord>> {
        let value = self.proxy.post_process_key_event()?;

        decode_records(records_array(&value)?)
    }

    /// Waits for the next signal addressed to this context.
    ///
    /// Answers `Ok(None)` immediately once [`Context::take_signals`] has moved
    /// the stream elsewhere, which is the frontend's arrangement; the polling
    /// form is what the phase-1 devtest uses.
    pub fn next_signal(&mut self, timeout: Option<Duration>) -> Result<Option<ContextSignal>> {
        match self.signals.as_mut() {
            Some(signals) => signals.next(timeout),
            None          => Ok(None),
        }
    }

    /// Detaches the signal stream, so a thread of its own can block on it.
    ///
    /// `None` on the second call: there is exactly one stream and whoever took
    /// it owns it. See [`SignalStream`] for why the split exists.
    pub fn take_signals(&mut self) -> Option<SignalStream> {
        self.signals.take()
    }
}

impl Drop for Context {
    /// Destroys the context in the daemon.
    ///
    /// Errors are swallowed: the common one is "the daemon already went away",
    /// which is exactly when there is nothing left to clean up, and a
    /// destructor is no place to fail anyway.
    fn drop(&mut self) {
        if let Err(e) = self.service.destroy() {
            tracing::debug!("destroying ibus context {}: {e}", self.path);
        }
    }
}

// --- Decoding ---

/// Extracts the `a(yv)` from a `PostProcessKeyEvent` read.
///
/// The daemon's introspection declares the property as `(a(yv))` while its
/// getter returns a bare `a(yv)` (`bus/inputcontext.c:260` against
/// `bus/inputcontext.c:1639-1648`), and IBus's own client decodes the bare
/// form. Both shapes are accepted here so that a daemon which one day makes
/// its XML honest does not break us.
fn records_array<'v>(value: &'v Value<'v>) -> Result<&'v Array<'v>> {
    match value {
        Value::Array(array) => Ok(array),
        Value::Structure(structure) if structure.fields().len() == 1 => {
            match &structure.fields()[0] {
                Value::Array(array) => Ok(array),
                other               => Err(Error::Decode {
                    what  : "PostProcessKeyEvent",
                    reason: format!("wrapped a {} rather than an array", other.value_signature()),
                }),
            }
        }
        other => Err(Error::Decode {
            what  : "PostProcessKeyEvent",
            reason: format!("expected a(yv), got {}", other.value_signature()),
        }),
    }
}

/// Turns the raw `(yv)` pairs into [`PostRecord`]s.
///
/// The `'u'` and `'m'` tags occupy two consecutive entries, so this is a small
/// state machine over the list rather than a map.
fn decode_records(array: &Array<'_>) -> Result<Vec<PostRecord>> {
    let mut pairs = Vec::with_capacity(array.len());
    for element in array.iter() {
        pairs.push(decode_pair(element)?);
    }

    let mut records = Vec::with_capacity(pairs.len());
    let mut index = 0;
    while index < pairs.len() {
        let (tag, text) = &pairs[index];
        index += 1;
        records.push(match tag {
            b'c' => PostRecord::Commit(text.clone()),
            b'd' => {
                let (offset, nchars) = parse_pair(&text.text, "delete-surrounding")?;
                PostRecord::DeleteSurrounding {
                    offset: offset as i32,
                    nchars: nchars,
                }
            }
            b'f' => {
                let (keyval, keycode, state) = parse_triple(&text.text, "forward-key")?;
                PostRecord::ForwardKey {
                    keyval : keyval,
                    keycode: keycode,
                    state  : state,
                }
            }
            b'h' => PostRecord::HidePreedit,
            b'r' => PostRecord::RequireSurrounding,
            b's' => PostRecord::ShowPreedit,
            b'u' | b'm' => {
                let Some((next_tag, position)) = pairs.get(index) else {
                    return Err(Error::Decode {
                        what  : "PostProcessKeyEvent",
                        reason: format!("tag '{}' without its position record", *tag as char),
                    });
                };
                if *next_tag != b'u' && *next_tag != b'm' {
                    return Err(Error::Decode {
                        what  : "PostProcessKeyEvent",
                        reason: format!(
                            "tag '{}' followed by '{}' rather than a position record",
                            *tag as char, *next_tag as char
                        ),
                    });
                }
                index += 1;

                // 'u' carries "cursor,visible"; 'm' appends the mode.
                let fields = split_numbers(&position.text, "update-preedit")?;
                let cursor = *fields.first().ok_or_else(|| Error::Decode {
                    what  : "PostProcessKeyEvent",
                    reason: format!("update-preedit position {:?} has no cursor", position.text),
                })?;
                let visible = fields.get(1).copied().unwrap_or(0) != 0;
                PostRecord::UpdatePreedit {
                    text   : text.clone(),
                    cursor : cursor as u32,
                    visible: visible,
                    mode   : fields.get(2).map(|mode| *mode as u32),
                }
            }
            other => {
                return Err(Error::Decode {
                    what  : "PostProcessKeyEvent",
                    reason: format!("unknown record tag {:?}", *other as char),
                });
            }
        });
    }

    Ok(records)
}

/// Decodes one `(yv)` entry into its tag and its `IBusText`.
fn decode_pair(element: &Value<'_>) -> Result<(u8, Text)> {
    let structure = match element {
        Value::Structure(structure) => structure,
        Value::Value(inner)         => match &**inner {
            Value::Structure(structure) => structure,
            other                       => return Err(not_a_pair(other)),
        },
        other => return Err(not_a_pair(other)),
    };

    let fields = structure.fields();
    let Some(Value::U8(tag)) = fields.first() else {
        return Err(Error::Decode {
            what  : "PostProcessKeyEvent",
            reason: "record tag is not a byte".to_string(),
        });
    };
    let Some(payload) = fields.get(1) else {
        return Err(Error::Decode {
            what  : "PostProcessKeyEvent",
            reason: "record has no payload".to_string(),
        });
    };

    Ok((*tag, Text::from_value(payload)?))
}

/// The "that was not a `(yv)`" error, shared by the two ways of failing.
fn not_a_pair(got: &Value<'_>) -> Error {
    Error::Decode {
        what  : "PostProcessKeyEvent",
        reason: format!("record is a {} rather than (yv)", got.value_signature()),
    }
}

/// Splits a printf-formatted record payload into its numbers.
fn split_numbers(text: &str, what: &'static str) -> Result<Vec<i64>> {
    text.split(',')
        .map(|field| {
            field.trim().parse::<i64>().map_err(|_| Error::Decode {
                what  : "PostProcessKeyEvent",
                reason: format!("{what} payload {text:?} is not a number list"),
            })
        })
        .collect()
}

/// Reads a two-number payload.
fn parse_pair(text: &str, what: &'static str) -> Result<(i64, u32)> {
    let fields = split_numbers(text, what)?;
    match fields.as_slice() {
        [first, second, ..] => Ok((*first, *second as u32)),
        _ => Err(Error::Decode {
            what  : "PostProcessKeyEvent",
            reason: format!("{what} payload {text:?} needs two numbers"),
        }),
    }
}

/// Reads a three-number payload.
fn parse_triple(text: &str, what: &'static str) -> Result<(u32, u32, u32)> {
    let fields = split_numbers(text, what)?;
    match fields.as_slice() {
        [first, second, third, ..] => Ok((*first as u32, *second as u32, *third as u32)),
        _ => Err(Error::Decode {
            what  : "PostProcessKeyEvent",
            reason: format!("{what} payload {text:?} needs three numbers"),
        }),
    }
}

/// Decodes one input-context signal message.
///
/// `Ok(None)` means "a signal on this interface that phase 1 has no use for",
/// which is not an error: the daemon may add signals and a client that fell
/// over on an unknown one would be fragile for no benefit.
fn decode_signal(member: &str, message: &zbus::message::Message) -> Result<Option<ContextSignal>> {
    let body = message.body();

    let signal = match member {
        "CommitText" => {
            let text: OwnedValue = body.deserialize()?;
            ContextSignal::CommitText(Text::from_value(&text)?)
        }
        "ForwardKeyEvent" => {
            let (keyval, keycode, state): (u32, u32, u32) = body.deserialize()?;
            ContextSignal::ForwardKeyEvent {
                keyval : keyval,
                keycode: keycode,
                state  : state,
            }
        }
        "UpdatePreeditText" => {
            let (text, cursor, visible): (OwnedValue, u32, bool) = body.deserialize()?;
            ContextSignal::UpdatePreedit {
                text   : Text::from_value(&text)?,
                cursor : cursor,
                visible: visible,
                mode   : None,
            }
        }
        "UpdatePreeditTextWithMode" => {
            let (text, cursor, visible, mode): (OwnedValue, u32, bool, u32) = body.deserialize()?;
            ContextSignal::UpdatePreedit {
                text   : Text::from_value(&text)?,
                cursor : cursor,
                visible: visible,
                mode   : Some(mode),
            }
        }
        "ShowPreeditText" => ContextSignal::ShowPreedit,
        "HidePreeditText" => ContextSignal::HidePreedit,
        "UpdateAuxiliaryText" => {
            let (text, visible): (OwnedValue, bool) = body.deserialize()?;
            ContextSignal::UpdateAuxiliary {
                text   : Text::from_value(&text)?,
                visible: visible,
            }
        }
        "ShowAuxiliaryText" => ContextSignal::ShowAuxiliary,
        "HideAuxiliaryText" => ContextSignal::HideAuxiliary,
        "UpdateLookupTable" => {
            let (table, visible): (OwnedValue, bool) = body.deserialize()?;
            ContextSignal::UpdateLookupTable {
                table  : LookupTable::from_value(&table)?,
                visible: visible,
            }
        }
        "ShowLookupTable"       => ContextSignal::ShowLookupTable,
        "HideLookupTable"       => ContextSignal::HideLookupTable,
        "PageUpLookupTable"     => ContextSignal::PageUpLookupTable,
        "PageDownLookupTable"   => ContextSignal::PageDownLookupTable,
        "CursorUpLookupTable"   => ContextSignal::CursorUpLookupTable,
        "CursorDownLookupTable" => ContextSignal::CursorDownLookupTable,
        "DeleteSurroundingText" => {
            let (offset, nchars): (i32, u32) = body.deserialize()?;
            ContextSignal::DeleteSurroundingText {
                offset: offset,
                nchars: nchars,
            }
        }
        "RequireSurroundingText" => ContextSignal::RequireSurroundingText,
        "RegisterProperties" => {
            let props: OwnedValue = body.deserialize()?;
            ContextSignal::RegisterProperties(PropList::from_value(&props)?)
        }
        "UpdateProperty" => {
            let prop: OwnedValue = body.deserialize()?;
            ContextSignal::UpdateProperty(Box::new(Property::from_value(&prop)?))
        }
        other => {
            tracing::debug!("ignoring unhandled ibus context signal {other}");
            return Ok(None);
        }
    };

    Ok(Some(signal))
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::StructureBuilder;

    /// Builds one `(yv)` drain record the way the daemon builds it: a tag byte
    /// and an `IBusText`, with the numeric payloads formatted into the text
    /// because there is nowhere else to put them.
    fn record(tag: u8, text: &str) -> Value<'static> {
        Value::Structure(
            StructureBuilder::new()
                .add_field(tag)
                .append_field(Value::Value(Box::new(
                    Text::plain(text).to_value().expect("encode the text"),
                )))
                .build()
                .expect("build the record"),
        )
    }

    /// Assembles records into the array the property returns.
    fn queue(records: Vec<Value<'static>>) -> Array<'static> {
        let signature = records[0].value_signature().clone();
        let mut array = Array::new(&signature);
        for record in records {
            array.append(record).expect("append");
        }

        array
    }

    #[test]
    fn decodes_a_commit() {
        let decoded = decode_records(&queue(vec![record(b'c', "日本語")])).expect("decode");
        assert_eq!(decoded, vec![PostRecord::Commit(Text::plain("日本語"))]);
    }

    /// `printf("%u,%u,%u", keyval, keycode, state)`, per
    /// `bus/inputcontext.c:1862-1866`.
    #[test]
    fn decodes_a_forwarded_key() {
        let decoded = decode_records(&queue(vec![record(b'f', "97,38,0")])).expect("decode");
        assert_eq!(
            decoded,
            vec![PostRecord::ForwardKey { keyval: 97, keycode: 38, state: 0 }]
        );
    }

    /// The offset is `%d` and genuinely negative in practice: engines delete
    /// text *behind* the cursor before committing a replacement.
    #[test]
    fn decodes_a_negative_delete_offset() {
        let decoded = decode_records(&queue(vec![record(b'd', "-2,3")])).expect("decode");
        assert_eq!(
            decoded,
            vec![PostRecord::DeleteSurrounding { offset: -2, nchars: 3 }]
        );
    }

    #[test]
    fn decodes_the_payload_less_tags() {
        let decoded = decode_records(&queue(vec![
            record(b's', ""),
            record(b'h', ""),
            record(b'r', ""),
        ]))
        .expect("decode");
        assert_eq!(
            decoded,
            vec![
                PostRecord::ShowPreedit,
                PostRecord::HidePreedit,
                PostRecord::RequireSurrounding,
            ]
        );
    }

    /// A preedit update is two records: the text, then its position. This is
    /// the shape a client with `ClientCommitPreedit` set actually receives.
    #[test]
    fn decodes_a_preedit_update_with_mode() {
        let decoded = decode_records(&queue(vec![
            record(b'm', "にほん"),
            record(b'm', "3,1,1"),
        ]))
        .expect("decode");
        assert_eq!(
            decoded,
            vec![PostRecord::UpdatePreedit {
                text   : Text::plain("にほん"),
                cursor : 3,
                visible: true,
                mode   : Some(PREEDIT_COMMIT),
            }]
        );
    }

    /// The `'u'` form has no mode field. We should not see it, since we always
    /// set `ClientCommitPreedit`, but decoding it costs one `Option`.
    #[test]
    fn decodes_a_preedit_update_without_mode() {
        let decoded =
            decode_records(&queue(vec![record(b'u', "abc"), record(b'u', "1,0")])).expect("decode");
        assert_eq!(
            decoded,
            vec![PostRecord::UpdatePreedit {
                text   : Text::plain("abc"),
                cursor : 1,
                visible: false,
                mode   : None,
            }]
        );
    }

    /// Order within a drain is the order the engine produced the effects, and
    /// a commit followed by a preedit clear is a different result from the
    /// reverse. The two-record pairing must not disturb it.
    #[test]
    fn preserves_order_across_the_paired_records() {
        let decoded = decode_records(&queue(vec![
            record(b'c', "committed"),
            record(b'm', ""),
            record(b'm', "0,0,0"),
            record(b'h', ""),
        ]))
        .expect("decode");
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], PostRecord::Commit(Text::plain("committed")));
        assert!(matches!(decoded[1], PostRecord::UpdatePreedit { .. }));
        assert_eq!(decoded[2], PostRecord::HidePreedit);
    }

    #[test]
    fn rejects_a_preedit_update_missing_its_position() {
        assert!(decode_records(&queue(vec![record(b'm', "text")])).is_err());
    }

    #[test]
    fn rejects_a_preedit_update_followed_by_the_wrong_tag() {
        assert!(decode_records(&queue(vec![record(b'm', "text"), record(b'c', "x")])).is_err());
    }

    #[test]
    fn rejects_an_unknown_tag() {
        assert!(decode_records(&queue(vec![record(b'z', "")])).is_err());
    }

    /// Both shapes of the property read: the bare array the daemon returns and
    /// the one-tuple its introspection declares.
    #[test]
    fn unwraps_either_property_shape() {
        let array = queue(vec![record(b's', "")]);
        let bare = Value::Array(array.try_clone().expect("clone"));
        assert_eq!(records_array(&bare).expect("bare").len(), 1);

        let wrapped = Value::Structure(
            StructureBuilder::new()
                .append_field(Value::Array(array))
                .build()
                .expect("build"),
        );
        assert_eq!(records_array(&wrapped).expect("wrapped").len(), 1);
    }

    #[test]
    fn describes_a_release_with_modifiers() {
        let described = describe_state(SHIFT_MASK | CONTROL_MASK | RELEASE_MASK);
        assert_eq!(described, "shift|control|release");
    }

    #[test]
    fn describes_the_capability_set_we_ask_for() {
        assert_eq!(
            describe_capabilities(CAPABILITIES),
            "preedit|auxiliary|lookup-table|focus|property|surrounding-text"
        );
    }
}
