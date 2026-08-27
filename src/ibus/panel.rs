//! The panel role: the engine-switch trigger and the global engine.
//!
//! `ibus-ui-gtk3` plays two parts, and retiring its Wayland IM means we
//! inherit both. [`super::context`] and [`crate::im::frontend`] are the bridge
//! half; this is the panel half — the one that tells ibus-daemon which key
//! combination switches input methods and then answers when it fires.
//!
//! # Why this is a connection of its own
//!
//! Everything here is addressed to the daemon's own object,
//! `/org/freedesktop/IBus`, and its signals are **broadcast by match rule**,
//! not unicast to a context. `bus_ibus_impl_emit_signal` builds a destination-
//! less signal and hands it to `bus_dbus_impl_dispatch_message_by_rule`
//! (`bus/ibusimpl.c:2477-2492`), which walks `dbus->rules` and sends to every
//! connection that registered a matching rule
//! (`bus/dbusimpl.c:1996-2019`). So a client only ever sees
//! `GlobalShortcutKeyResponded` if it called `AddMatch` — the registering
//! client gets no special treatment, and neither does the focused context's
//! connection.
//!
//! That makes the panel's message stream a different filter from the input
//! context's, and [`super::SignalStream`] drops anything not addressed to its
//! own object path. Rather than teach one stream two vocabularies, the panel
//! opens a second connection to the same daemon and reads its own. The daemon
//! does not care how many connections a process holds; contexts are tied to
//! the connection that created them and this one creates none.
//!
//! # What is deliberately not here
//!
//! Reading the registration back. `GlobalShortcutKeys` introspects as a
//! readable property but the daemon implements no getter for it
//! (`bus/ibusimpl.c:2142-2147`, phase-1 finding 6), so what we registered is
//! only knowable from our own copy.

use std::time::Duration;

use super::bus::ShortcutKeys;
use super::text::EngineDesc;
use super::{Address, Bus, Result, Signals};

// --- Binding types ---

/// `IBUS_BUS_GLOBAL_BINDING_TYPE_IME_SWITCHER`, the `y` in the
/// `GlobalShortcutKeys` property and in `GlobalShortcutKeyResponded`.
///
/// The enum is `IBusBusGlobalBindingType` and it lives in `ibusbus.h:79-84`,
/// not in `ibustypes.h` and not under a `…GLOBAL_SHORTCUT_KEYS…` name: `ANY`
/// is 0, this is 1, `EMOJI_TYPING` is 2. The other two are not declared here
/// because neither can be registered — the daemon's setter switches on the
/// type with one case for the switcher and a `default` that frees the keys it
/// was handed (`bus/ibusimpl.c:2039-2050`), so in 1.5.34 anything else stores
/// nothing and fires nothing.
pub const BINDING_TYPE_IME_SWITCHER: u8 = 1;

// --- Shortcuts ---

/// One key combination registered as a global shortcut.
///
/// The wire form is a `(keyval, keycode, state)` triple, but the daemon does
/// not use the middle field as a keycode at all: it reads it as the
/// backward flag, `is_backward = ime_switcher_keys[i].keycode != 0`
/// (`bus/ibusimpl.c:2622`), and `ibus-ui-gtk3` writes it as
/// `kb.reverse ? 1 : 0` (`ui/gtk3/panel.vala:549`). Nothing ever compares it
/// against a hardware keycode, so a real one here would silently mean
/// "backward".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    /// X keysym of the non-modifier key.
    pub keyval  : u32,
    /// IBus modifier bits, already normalised the way the daemon will
    /// normalise the incoming event before comparing: see
    /// [`crate::im::switcher`].
    pub state   : u32,
    /// Whether this combination cycles backwards through the engine list.
    pub backward: bool,
}

impl Shortcut {
    /// The wire triple, with the backward flag in the keycode slot.
    pub fn to_triple(self) -> (u32, u32, u32) {
        (self.keyval, u32::from(self.backward), self.state)
    }
}

/// Packs shortcuts into the `(ya(uuu))` value the `GlobalShortcutKeys`
/// property takes.
///
/// The C API takes a zero-terminated array and serialises it with
/// `for (i = 0; keys[i].keyval; ++i)` (`src/ibusbus.c:2480-2500`), so the
/// terminator never reaches the wire and there is nothing to append here. The
/// daemon rejects an empty list (`g_return_val_if_fail (size > 0, FALSE)`,
/// `bus/ibusimpl.c:2025`), which is why registration is skipped rather than
/// attempted when nothing parsed.
pub fn shortcut_keys(binding: u8, shortcuts: &[Shortcut]) -> ShortcutKeys {
    (
        binding,
        shortcuts.iter().map(|shortcut| shortcut.to_triple()).collect(),
    )
}

// --- Signals ---

/// The interface and object the panel's signals arrive on.
const DAEMON_INTERFACE: &str = "org.freedesktop.IBus";

/// The daemon's own object path.
const DAEMON_PATH: &str = "/org/freedesktop/IBus";

/// The match rule that makes the daemon's broadcasts reach this connection.
///
/// `sender` names the well-known name rather than `:1.0`; ibus-daemon rewrites
/// exactly that value to its own unique name when it parses the rule
/// (`bus/dbusimpl.c:976-978`), which is the case libibus's own
/// `ibus_bus_watch_ibus_signal` relies on (`src/ibusbus.c:1203-1230`).
const MATCH_RULE: &str = "type='signal',sender='org.freedesktop.IBus',\
                          interface='org.freedesktop.IBus',path='/org/freedesktop/IBus'";

/// Something the daemon announced to whoever is listening.
#[derive(Debug, Clone)]
pub enum PanelSignal {
    /// A registered global shortcut fired, and the daemon has already consumed
    /// the key — `_ic_process_key_event` answers `TRUE` and returns before the
    /// engine ever sees it (`bus/inputcontext.c:1088-1099`), so the key does
    /// not also type a space.
    ShortcutKeyResponded {
        /// Which binding fired; compare against [`BINDING_TYPE_IME_SWITCHER`].
        binding : u8,
        /// The keysym that triggered it.
        keyval  : u32,
        /// The keycode as the client reported it, not the backward flag the
        /// registration put in this slot.
        keycode : u32,
        /// Modifier bits, with `RELEASE_MASK` set on the release edge.
        state   : u32,
        /// Whether the entry that matched was registered as backward.
        backward: bool,
    },
    /// The global engine changed, by whatever means — including our own
    /// `SetGlobalEngine`, so this is the confirmation as well as the news.
    GlobalEngineChanged {
        /// The engine id now in effect.
        name: String,
    },
    /// The engine registry was rebuilt, so cached descriptions are stale.
    RegistryChanged,
}

impl std::fmt::Display for PanelSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PanelSignal::ShortcutKeyResponded { binding, keyval, keycode, state, backward } => {
                write!(
                    f,
                    "GlobalShortcutKeyResponded type={binding} keyval={keyval:#x} \
                     keycode={keycode} state={} backward={backward}",
                    super::describe_state(*state)
                )
            }
            PanelSignal::GlobalEngineChanged { name } => {
                write!(f, "GlobalEngineChanged {name}")
            }
            PanelSignal::RegistryChanged => write!(f, "RegistryChanged"),
        }
    }
}

/// The decoded daemon-signal stream, detachable from the [`Panel`] so it can
/// block on a thread of its own.
///
/// Same shape and same reason as [`super::SignalStream`]: zbus buffers a
/// bounded queue per stream and stalls the connection when it fills, so
/// something has to stay parked in `next` while the rest of the panel makes
/// calls.
pub struct PanelStream {
    /// The raw message stream underneath.
    signals: Signals,
}

impl PanelStream {
    /// Waits for the next signal from the daemon's own object.
    ///
    /// `None` means the timeout expired with nothing for us, or the connection
    /// closed — which is also how the frontend learns ibus-daemon died.
    pub fn next(&mut self, timeout: Option<Duration>) -> Result<Option<PanelSignal>> {
        loop {
            let Some(message) = self.signals.next(timeout)? else {
                return Ok(None);
            };

            let header = message.header();
            if message.message_type() != zbus::message::Type::Signal {
                continue;
            }
            if header.path().map(|path| path.as_str()) != Some(DAEMON_PATH) {
                continue;
            }
            if header.interface().map(|name| name.as_str()) != Some(DAEMON_INTERFACE) {
                continue;
            }
            let Some(member) = header.member() else {
                continue;
            };

            match member.as_str() {
                "GlobalShortcutKeyResponded" => {
                    let (binding, keyval, keycode, state, backward): (u8, u32, u32, u32, bool) =
                        message.body().deserialize()?;
                    return Ok(Some(PanelSignal::ShortcutKeyResponded {
                        binding : binding,
                        keyval  : keyval,
                        keycode : keycode,
                        state   : state,
                        backward: backward,
                    }));
                }
                "GlobalEngineChanged" => {
                    let (name,): (String,) = message.body().deserialize()?;
                    return Ok(Some(PanelSignal::GlobalEngineChanged { name: name }));
                }
                "RegistryChanged" => return Ok(Some(PanelSignal::RegistryChanged)),
                _                 => continue,
            }
        }
    }
}

// --- The panel ---

/// A connection to ibus-daemon that plays the panel's part.
pub struct Panel {
    /// The connection and the typed daemon calls on it.
    bus    : Bus,
    /// The signal stream, until [`Panel::take_signals`] moves it to a thread.
    signals: Option<PanelStream>,
}

impl Panel {
    /// Connects to the daemon and subscribes to its broadcasts.
    ///
    /// The stream is created before the `AddMatch`, not after: the match rule
    /// is what makes signals start arriving, so anything already buffered by
    /// then is ours to read rather than ours to have missed.
    pub fn connect(address: Address) -> Result<Self> {
        let bus = Bus::connect_to(address)?;
        let signals = Signals::new(bus.connection());

        bus.connection().call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "AddMatch",
            &(MATCH_RULE,),
        )?;

        Ok(Self {
            bus    : bus,
            signals: Some(PanelStream { signals: signals }),
        })
    }

    /// Where this panel connected, for the log line that says so.
    pub fn address(&self) -> String {
        self.bus.address().source.to_string()
    }

    /// Hands the signal stream to whoever will block on it. Once.
    pub fn take_signals(&mut self) -> Option<PanelStream> {
        self.signals.take()
    }

    /// Registers the engine-switch trigger.
    ///
    /// Irreversible for the life of the daemon, and global: see
    /// [`Bus::set_global_shortcut_keys`].
    pub fn register(&self, shortcuts: &[Shortcut]) -> Result<()> {
        self.bus
            .set_global_shortcut_keys(shortcut_keys(BINDING_TYPE_IME_SWITCHER, shortcuts))
    }

    /// The engine in effect right now, by id.
    pub fn global_engine(&self) -> Result<String> {
        Ok(self.bus.global_engine()?.name)
    }

    /// Switches the engine for every context at once.
    ///
    /// The only switch there is while `use-global-engine` is set, which is the
    /// default and which makes per-context `SetEngine` an error (phase-1
    /// finding 9). The engine attaches to a context on its next `FocusIn`.
    pub fn set_global_engine(&self, name: &str) -> Result<()> {
        self.bus.set_global_engine(name)
    }

    /// Looks one engine up in the registry, for its display name and symbol.
    ///
    /// `None` rather than an error when the registry does not know the name:
    /// `GetEnginesByNames` drops unknown ids instead of failing, and an engine
    /// the user configured but did not install is a thing to report, not to
    /// crash on.
    pub fn describe_engine(&self, name: &str) -> Result<Option<EngineDesc>> {
        Ok(self.bus.engines_by_names(&[name])?.into_iter().next())
    }

    /// Looks a whole list up at once, for validating a configured cycle.
    ///
    /// The registry here has 977 entries, so this is the difference between
    /// one call and reading all of them: `GetEnginesByNames` filters
    /// server-side and silently drops ids it does not know, which makes a
    /// short result the report that an engine is missing.
    pub fn describe_engines(&self, names: &[&str]) -> Result<Vec<EngineDesc>> {
        self.bus.engines_by_names(names)
    }
}

impl std::fmt::Debug for Panel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Panel")
            .field("address", &self.address())
            .finish_non_exhaustive()
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Type, serialized::Context as EncodingContext, to_bytes};

    /// The property's declared signature is `(ya(uuu))`
    /// (`bus/ibusimpl.c:200`), and nothing checks that our Rust type agrees
    /// until the daemon rejects the `Set` at runtime.
    #[test]
    fn shortcut_keys_signature_matches_the_property() {
        assert_eq!(ShortcutKeys::SIGNATURE.to_string(), "(ya(uuu))");
    }

    /// The backward flag rides in the keycode slot, so a round trip has to
    /// preserve field *positions*, not just field values.
    #[test]
    fn shortcut_keys_round_trip() {
        let shortcuts = [
            Shortcut { keyval: 0x0020, state: 0x0c, backward: false },
            Shortcut { keyval: 0x0020, state: 0x0d, backward: true },
        ];
        let keys = shortcut_keys(BINDING_TYPE_IME_SWITCHER, &shortcuts);

        let context = EncodingContext::new_dbus(zbus::zvariant::LE, 0);
        let encoded = to_bytes(context, &keys).expect("encoding");
        let (decoded, _): (ShortcutKeys, _) = encoded.deserialize().expect("decoding");

        assert_eq!(decoded.0, BINDING_TYPE_IME_SWITCHER);
        assert_eq!(
            decoded.1,
            vec![(0x0020, 0, 0x0c), (0x0020, 1, 0x0d)],
            "the backward flag must land in the middle field"
        );
    }

    /// An empty list is refused by the daemon rather than clearing the
    /// registration, so the encoder must not be the thing that produces one.
    #[test]
    fn no_shortcuts_encodes_to_an_empty_array() {
        let keys = shortcut_keys(BINDING_TYPE_IME_SWITCHER, &[]);
        assert!(keys.1.is_empty());
    }
}
