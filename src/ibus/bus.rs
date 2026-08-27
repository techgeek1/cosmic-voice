//! The connection to ibus-daemon and its `org.freedesktop.IBus` object.
//!
//! ibus-daemon is both a bus *implementation* and a peer on it: it listens on
//! the private socket, speaks the `org.freedesktop.DBus` interface itself
//! (`bus/dbusimpl.c`), and separately owns the well-known name
//! `org.freedesktop.IBus` at `/org/freedesktop/IBus`. So this is an ordinary
//! bus connection — SASL EXTERNAL over the unix socket, a real `Hello`, a
//! unique name back — and not a peer-to-peer one. Getting that wrong is a
//! silent hang rather than an error, which is why the connection is built with
//! `Builder::address` and never `.p2p()`.
//!
//! Signals from the daemon to a context are *unicast*, addressed to our unique
//! name, so no `AddMatch` is needed to receive them; see [`super::Signals`].

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::text::{EngineDesc, Serializable};
use super::{Address, Context, Error, Result};

// --- The daemon interface ---

/// A registered global shortcut: a binding type (the only one defined is
/// `IME_SWITCHER`) plus the `(keyval, keycode, state)` triples that trigger it.
///
/// Aliased because the tuple appears in three signatures and reads as noise
/// inline.
pub type ShortcutKeys = (u8, Vec<(u32, u32, u32)>);

/// `org.freedesktop.IBus`, the daemon's own object.
///
/// The whole interface is bound, against ibus 1.5.34 and cross-checked with
/// live introspection, but phase 1 only *calls* the read-only half plus
/// `CreateInputContext`. Everything else is here so that phases 2 and 4 do not
/// have to re-derive it, with the caveats worth knowing before using it:
///
/// - `Exit(b)` must never be called from this process. See its doc comment.
/// - `SetGlobalEngine`, `SetGlobalShortcutKeys` and `PreloadEngines` are panel
///   duties, phase 4 of `docs/multiplexer.md`.
/// - `GlobalShortcutKeys` introspects as readable but the daemon implements no
///   getter for it — `bus/ibusimpl.c:2142-2147` lists the readable properties
///   and it is not among them — so reading it fails at runtime.
///   `devtest ibus-info` reports that mismatch rather than hiding it.
/// - The signals are broadcast, not unicast, so a consumer needs an `AddMatch`
///   as well as this declaration. Nothing in phase 1 acts on one.
#[zbus::proxy(
    interface       = "org.freedesktop.IBus",
    default_service = "org.freedesktop.IBus",
    default_path    = "/org/freedesktop/IBus",
    gen_async       = false
)]
pub trait Daemon {
    /// Creates an input context and returns its object path. The client name
    /// is not decorative; see [`super::CLIENT_NAME`].
    fn create_input_context(&self, client_name: &str) -> zbus::Result<OwnedObjectPath>;

    /// Looks up engine descriptions by id. Unknown names are skipped rather
    /// than erroring, so the result may be shorter than the request.
    fn get_engines_by_names(&self, names: &[&str]) -> zbus::Result<Vec<OwnedValue>>;

    /// Echoes its argument back. The cheapest possible liveness check that
    /// exercises the codec path as well as the socket.
    fn ping(&self, data: &Value<'_>) -> zbus::Result<OwnedValue>;

    /// Switches the engine for every context at once. Phase 4 calls this to
    /// cycle engines on the switch trigger; nothing before then should, since
    /// it changes what the user is typing with.
    fn set_global_engine(&self, name: &str) -> zbus::Result<()>;

    /// Stops or restarts the user's ibus-daemon.
    ///
    /// Bound for completeness and never to be called from this process. The
    /// multiplexer's failure story rests on never disturbing IBus's lifecycle:
    /// if we exit the daemon, every application's input method dies with it,
    /// and nothing here is ever a good enough reason.
    fn exit(&self, restart: bool) -> zbus::Result<()>;

    /// Announces an engine component to the registry. Engine-side API; we are
    /// a client and will never call it.
    fn register_component(&self, component: &Value<'_>) -> zbus::Result<()>;

    /// The bus address the daemon believes it is listening on. Worth reading
    /// even though we had to know it to get here: it proves we reached the
    /// daemon that wrote the address file we read.
    ///
    /// Every property here is declared uncached. zbus's default is to cache,
    /// which means a `GetAll` on first access plus a `PropertiesChanged`
    /// subscription — and IBus emits no property-change signals at all, so
    /// caching would hand out stale engine state forever.
    #[zbus(property(emits_changed_signal = "false"))]
    fn address(&self) -> zbus::Result<String>;

    /// The context the daemon currently considers focused, as an object path.
    #[zbus(property(emits_changed_signal = "false"))]
    fn current_input_context(&self) -> zbus::Result<OwnedObjectPath>;

    /// Every engine the registry knows about, as `IBusEngineDesc` values.
    #[zbus(property(emits_changed_signal = "false"))]
    fn engines(&self) -> zbus::Result<Vec<OwnedValue>>;

    /// The engines the user has actually enabled, in preference order. Phase 4
    /// cycles through this list on the engine-switch trigger.
    #[zbus(property(emits_changed_signal = "false"))]
    fn active_engines(&self) -> zbus::Result<Vec<OwnedValue>>;

    /// The engine in effect right now.
    #[zbus(property(emits_changed_signal = "false"))]
    fn global_engine(&self) -> zbus::Result<OwnedValue>;

    /// Whether the daemon expects clients to render preedit themselves.
    ///
    /// Directly load-bearing: preedit is only delivered to a client when
    /// `CAP_PREEDIT_TEXT` is set *and* either this is true or the client did
    /// not ask for `CAP_FOCUS` (`bus/inputcontext.c:378-383`,
    /// `PREEDIT_CONDITION`). With it off and both capabilities set, the daemon
    /// routes preedit to a panel that no longer exists.
    #[zbus(property(emits_changed_signal = "false"))]
    fn embed_preedit_text(&self) -> zbus::Result<bool>;

    /// Switches preedit routing for every context. Changing it affects other
    /// clients, so phase 1 only ever reads it.
    #[zbus(property(emits_changed_signal = "false"))]
    fn set_embed_preedit_text(&self, embed: bool) -> zbus::Result<()>;

    /// The registered engine-switch shortcut: a binding type plus a list of
    /// `(keyval, keycode, state)` triples. Write-only in practice, see above.
    #[zbus(property(emits_changed_signal = "false"))]
    fn global_shortcut_keys(&self) -> zbus::Result<ShortcutKeys>;

    /// Registers the engine-switch trigger, which is what makes the daemon
    /// consume it inside `ProcessKeyEvent` and answer with
    /// `GlobalShortcutKeyResponded`. Phase 4; note that calling it is also what
    /// makes the daemon consider this a "Wayland session", with the
    /// consequences described on [`super::CLIENT_NAME`].
    #[zbus(property(emits_changed_signal = "false"))]
    fn set_global_shortcut_keys(&self, keys: ShortcutKeys) -> zbus::Result<()>;

    /// Replaces the user's enabled-engine list. Write-only, and a user setting
    /// rather than ours to touch.
    #[zbus(property(emits_changed_signal = "false"))]
    fn set_preload_engines(&self, engines: &[&str]) -> zbus::Result<()>;

    /// The global engine changed, by whatever means.
    #[zbus(signal)]
    fn global_engine_changed(&self, name: &str) -> zbus::Result<()>;

    /// A registered global shortcut fired. The daemon has already consumed the
    /// key; phase 4 responds by cycling engines.
    #[zbus(signal)]
    fn global_shortcut_key_responded(
        &self,
        binding_type: u8,
        keyval      : u32,
        keycode     : u32,
        state       : u32,
        backward    : bool,
    ) -> zbus::Result<()>;

    /// The engine registry was rebuilt, so cached descriptions are stale.
    #[zbus(signal)]
    fn registry_changed(&self) -> zbus::Result<()>;
}

// --- The connection ---

/// A live connection to ibus-daemon's private bus.
///
/// Holding one of these is cheap and the daemon does not care how many exist,
/// but every [`Context`] is tied to the connection it was created on: the
/// daemon destroys a context when its owning connection drops.
pub struct Bus {
    /// Where we connected, kept for diagnostics and reconnection.
    address   : Address,
    /// The connection itself. Contexts clone it for their signal streams, so
    /// it must outlive them — which it does, since [`Context`] holds one.
    connection: zbus::blocking::Connection,
    /// Proxy for the daemon's own object.
    daemon    : DaemonProxy<'static>,
}

impl Bus {
    /// Discovers the daemon and connects to it.
    pub fn connect() -> Result<Self> {
        Self::connect_to(Address::discover()?)
    }

    /// Connects to a daemon whose address the caller already knows.
    pub fn connect_to(address: Address) -> Result<Self> {
        let connection = zbus::blocking::connection::Builder::address(address.address.as_str())?
            .build()?;
        let daemon = DaemonProxy::new(&connection)?;

        Ok(Self {
            address   : address,
            connection: connection,
            daemon    : daemon,
        })
    }

    /// Where this connection came from.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// The unique name the bus assigned us. Proof that `Hello` happened, and
    /// the name the daemon unicasts our signals to.
    pub fn unique_name(&self) -> String {
        self.connection
            .unique_name()
            .map(|name| name.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    }

    /// The address the daemon reports for itself.
    pub fn daemon_address(&self) -> Result<String> {
        Ok(self.daemon.address()?)
    }

    /// The object path of the context the daemon currently considers focused.
    pub fn current_input_context(&self) -> Result<String> {
        Ok(self.daemon.current_input_context()?.to_string())
    }

    /// Whether the daemon will hand preedit to clients. See
    /// [`DaemonProxy::embed_preedit_text`] for why this matters.
    pub fn embed_preedit_text(&self) -> Result<bool> {
        Ok(self.daemon.embed_preedit_text()?)
    }

    /// The engine in effect, decoded.
    pub fn global_engine(&self) -> Result<EngineDesc> {
        let value = self.daemon.global_engine()?;

        EngineDesc::from_value(&value)
    }

    /// Switches the engine in effect for every context.
    ///
    /// With `use-global-engine` set (the default), the daemon refuses
    /// per-context `SetEngine`, so this is the only way to pick an engine —
    /// and it changes it for the user's real windows too. Callers that switch
    /// for their own purposes must put the previous engine back.
    pub fn set_global_engine(&self, name: &str) -> Result<()> {
        Ok(self.daemon.set_global_engine(name)?)
    }

    /// Every engine in the registry, decoded.
    pub fn engines(&self) -> Result<Vec<EngineDesc>> {
        decode_engines(self.daemon.engines()?)
    }

    /// The engines the user enabled, decoded.
    pub fn active_engines(&self) -> Result<Vec<EngineDesc>> {
        decode_engines(self.daemon.active_engines()?)
    }

    /// The registered engine-switch shortcut. Expected to fail today; see
    /// [`DaemonProxy`].
    pub fn global_shortcut_keys(&self) -> Result<ShortcutKeys> {
        Ok(self.daemon.global_shortcut_keys()?)
    }

    /// Registers the engine-switch trigger, taking on the panel's role.
    ///
    /// Two things happen inside the daemon as a result, both of them global
    /// and neither of them undoable: `ProcessKeyEvent` starts consuming the
    /// trigger and answering `GlobalShortcutKeyResponded`
    /// (`bus/ibusimpl.c:2591-2664`), and `bus_ibus_impl_is_wayland_session`
    /// starts returning true (`:2666-2672`), which arms the `ignore_focus_out`
    /// trap for every client whose name does not begin `wayland`. See
    /// [`super::CLIENT_NAME`]. There is no unregister.
    pub fn set_global_shortcut_keys(&self, keys: ShortcutKeys) -> Result<()> {
        Ok(self.daemon.set_global_shortcut_keys(keys)?)
    }

    /// Looks up engines by id, decoded. Names the registry does not know are
    /// dropped, so compare lengths if that matters.
    pub fn engines_by_names(&self, names: &[&str]) -> Result<Vec<EngineDesc>> {
        decode_engines(self.daemon.get_engines_by_names(names)?)
    }

    /// Round-trips a string through the daemon and returns what came back.
    pub fn ping(&self, payload: &str) -> Result<String> {
        let echoed = self.daemon.ping(&Value::from(payload))?;
        match &*echoed {
            Value::Str(text) => Ok(text.to_string()),
            other            => Err(Error::Decode {
                what  : "Ping",
                reason: format!("echoed a {} back", other.value_signature()),
            }),
        }
    }

    /// The connection underneath, for the parts of this module that need a
    /// raw message stream or a call the proxies do not cover.
    ///
    /// Crate-private on purpose: a bare connection invites bypassing the typed
    /// wrappers above, and the only legitimate user is [`super::Panel`], which
    /// needs `AddMatch` and a second [`super::Signals`] on the same socket.
    pub(super) fn connection(&self) -> &zbus::blocking::Connection {
        &self.connection
    }

    /// Creates an input context and puts it into the state the multiplexer
    /// needs. See [`Context::create`] for what that state is.
    pub fn create_input_context(&self, client_name: &str) -> Result<Context> {
        let path = self.daemon.create_input_context(client_name)?;

        Context::create(&self.connection, path)
    }
}

/// Decodes a list of `IBusEngineDesc` variants.
fn decode_engines(values: Vec<OwnedValue>) -> Result<Vec<EngineDesc>> {
    values
        .iter()
        .map(|value| EngineDesc::from_value(value))
        .collect()
}
