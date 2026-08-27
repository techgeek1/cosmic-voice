//! Panel duties: registering the engine-switch trigger and cycling engines.
//!
//! [`super::link`] owns the input context — the half of the panel's job that
//! puts keys into an engine. This is the other half: the one that decides
//! *which* engine, and the reason Super+Space does something other than typing
//! a space. Without a registered trigger the daemon has no `ime_switcher_keys`
//! and `bus_ibus_impl_process_key_event` returns false for every key
//! (`bus/ibusimpl.c:2599-2601`), so the combination falls through to the
//! engine as an ordinary space.
//!
//! # The three translations
//!
//! A trigger travels through three notations before the daemon can compare it
//! against a key event, and each one loses something:
//!
//! 1. **dconf** holds GTK accelerator strings —
//!    `org.freedesktop.ibus.general.hotkey triggers`, `['<Control><Alt>space']`
//!    on this machine, `['<Super>space']` by schema default.
//! 2. **[`parse_accelerator`]** turns one into a keysym plus IBus modifier
//!    bits. The bits are not a straight transcription: the daemon masks the
//!    incoming event with `IBUS_MODIFIER_FILTER`, which *excludes* the
//!    `SUPER`, `HYPER` and `META` aliases (`ibustypes.h:386-398`), after first
//!    rewriting `SUPER` to `MOD4` (`bus/ibusimpl.c:2615-2619`). A registration
//!    that spells Super as `IBUS_SUPER_MASK` can therefore never match, so
//!    this maps the virtual modifiers onto the real ones the way an X keymap
//!    does — which is also what `ibus-ui-gtk3` gets from
//!    `gdk_keymap_map_virtual_modifiers` (`ui/gtk3/bindingcommon.vala:73-84`).
//! 3. **[`crate::ibus::Shortcut`]** is the wire form, in which the backward
//!    flag rides in the keycode field.
//!
//! # Forward and backward
//!
//! One accelerator becomes up to two registrations: itself, and itself plus
//! Shift as the backward cycle, exactly as the panel does
//! (`ui/gtk3/bindingcommon.vala:112-123`) — skipped when the accelerator
//! already carries Shift, because the two would then be the same combination
//! with contradictory directions.
//!
//! # Press, not release
//!
//! The daemon emits `GlobalShortcutKeyResponded` twice per use of a modified
//! trigger: once on the press, and once when the last modifier comes up
//! (`bus/ibusimpl.c:2626-2647`). `ibus-ui-gtk3` uses the pair to run a switcher
//! popup — press moves the selection, release commits it. We cycle on the
//! press and ignore the release, which is what "cycle-only v1" means and what
//! keeps the press/release state client-side, away from the delivery-latency
//! workaround the daemon documents at `bus/ibusimpl.c:2650-2660`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use calloop::channel::Sender;
use tokio::sync::mpsc;
use xkbcommon::xkb;

use super::link::Upstream;
use crate::ibus::{
    Address, BINDING_TYPE_IME_SWITCHER, CONTROL_MASK, MOD1_MASK, MOD2_MASK, MOD3_MASK, MOD4_MASK,
    MOD5_MASK, MODIFIER_FILTER, Panel, PanelSignal, PanelStream, RELEASE_MASK, SHIFT_MASK, Shortcut,
};

/// How long to wait before trying a daemon that was not there again.
///
/// The same three seconds [`super::link`] uses, and for the same reason. The
/// two clocks are separate rather than shared because the panel connection and
/// the context connection fail independently — the daemon can accept one and
/// refuse the other only in ways that would be a bug, but a bug that stalls
/// both is worse than one that stalls neither.
const RETRY_INTERVAL: Duration = Duration::from_secs(3);

// --- Events out ---

/// Something the IM stack wants the rest of the process to know.
///
/// It exists as an enum rather than a callback because the applet forwards
/// these over the existing IPC channel and a channel wants a message type; the
/// frontend logs every one of them at info whether or not anybody is
/// listening, so the trace is complete even with no receiver attached.
///
/// Two producers: this module, which knows which engine is in effect, and
/// [`super::supervisor`], which knows whether a frontend is running at all.
/// One consumer — the dictation engine — folds both into the single
/// [`crate::ipc::InputMethodState`] the panel popup renders, which is why they
/// share one channel rather than having one each.
#[derive(Debug, Clone)]
pub enum ImEvent {
    /// The global input engine changed. Emitted on our own switches and on
    /// anybody else's, because the daemon does not distinguish and the panel
    /// applet wants to show what is actually in effect.
    EngineChanged {
        /// Engine id, e.g. `mozc-jp`.
        name    : String,
        /// Short status-area symbol, e.g. `あ`. Often empty for xkb engines.
        symbol  : String,
        /// Human-readable name, e.g. `Mozc`.
        longname: String,
    },
    /// The frontend bound the seat's input-method slot and is running.
    Bound,
    /// The frontend will not bind, and the user has to do something about it.
    /// The only cause today is IBus's own Wayland bridge still holding the
    /// slot, which is what the cutover retires.
    Blocked {
        /// What to put in front of the user, in one line.
        reason: String,
    },
    /// The frontend stopped. The supervisor restarts it after a backoff; this
    /// exists so the popup says so rather than silently showing a working
    /// input method that is not there.
    Stopped {
        /// Why it stopped, or "the loop exited" for a clean return.
        reason: String,
    },
}

impl std::fmt::Display for ImEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImEvent::EngineChanged { name, symbol, longname } => {
                write!(f, "engine {name}")?;
                if !longname.is_empty() && longname != name {
                    write!(f, " ({longname})")?;
                }
                if !symbol.is_empty() {
                    write!(f, " [{symbol}]")?;
                }
                Ok(())
            }
            ImEvent::Bound              => write!(f, "input method bound"),
            ImEvent::Blocked { reason } => write!(f, "input method blocked: {reason}"),
            ImEvent::Stopped { reason } => write!(f, "input method stopped: {reason}"),
        }
    }
}

// --- Accelerator parsing ---

/// One parsed accelerator: the key, and the modifiers as the daemon will see
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    /// X keysym of the non-modifier key.
    pub keyval: u32,
    /// IBus modifier bits, already normalised into [`MODIFIER_FILTER`].
    pub state : u32,
}

/// Parses one GTK accelerator string.
///
/// `gtk_accelerator_parse`'s grammar: zero or more `<Name>` modifier tokens,
/// case-insensitive, followed by a key name that
/// `xkb::keysym_from_name` resolves. Returns `None` for anything it cannot
/// make sense of, because a trigger that silently becomes "no modifiers plus
/// keysym 0" would register a shortcut that matches nothing and hide the
/// typo.
///
/// The modifier mapping is deliberately lossy in one direction: `<Super>` and
/// `<Hyper>` both become `MOD4` and `<Meta>` becomes `MOD1`, which is where a
/// standard X keymap puts them. The aliases they name are stripped by
/// `IBUS_MODIFIER_FILTER` before the daemon compares, so registering them
/// literally would produce a binding that can never fire.
pub fn parse_accelerator(accelerator: &str) -> Option<Trigger> {
    let mut rest = accelerator.trim();
    let mut state = 0;

    while let Some(remainder) = rest.strip_prefix('<') {
        let (name, remainder) = remainder.split_once('>')?;
        state |= match name.to_ascii_lowercase().as_str() {
            "shift"                    => SHIFT_MASK,
            "control" | "ctrl" | "ctl" => CONTROL_MASK,
            // GTK's portable spelling of "the accelerator modifier", which on
            // anything that is not macOS is Control.
            "primary"                  => CONTROL_MASK,
            "alt" | "mod1"             => MOD1_MASK,
            "mod2"                     => MOD2_MASK,
            "mod3"                     => MOD3_MASK,
            "mod4" | "super" | "hyper" => MOD4_MASK,
            "mod5"                     => MOD5_MASK,
            "meta"                     => MOD1_MASK,
            // `<Release>` is meaningful to `gtk_accelerator_parse` but not to
            // a global shortcut: the daemon derives the edge from the event's
            // own RELEASE_MASK and compares only the filtered modifiers.
            "release"                  => 0,
            _                          => return None,
        };
        rest = remainder;
    }

    if rest.is_empty() {
        return None;
    }

    // Exact first, because keysym names are case-sensitive and `a` and `A` are
    // different symbols; the insensitive retry is for the spellings people
    // actually write in a config file, like `space` versus `Space`.
    let mut keysym = xkb::keysym_from_name(rest, xkb::KEYSYM_NO_FLAGS);
    if keysym == xkb::Keysym::NoSymbol {
        keysym = xkb::keysym_from_name(rest, xkb::KEYSYM_CASE_INSENSITIVE);
    }
    if keysym == xkb::Keysym::NoSymbol {
        return None;
    }

    Some(Trigger {
        keyval: keysym.raw(),
        state : state & MODIFIER_FILTER,
    })
}

/// Turns configured accelerators into the registrations for the IME switcher.
///
/// Each accelerator contributes a forward entry, and — unless it already
/// carries Shift — a Shift-modified backward one. Unparseable entries are
/// warned about and skipped rather than aborting the lot: one bad line in a
/// config file should cost that line, not the feature.
pub fn switcher_shortcuts(accelerators: &[String]) -> Vec<Shortcut> {
    let mut shortcuts = Vec::new();

    for accelerator in accelerators {
        let Some(trigger) = parse_accelerator(accelerator) else {
            tracing::warn!("ignoring unparseable engine-switch trigger {accelerator:?}");
            continue;
        };

        shortcuts.push(Shortcut {
            keyval  : trigger.keyval,
            state   : trigger.state,
            backward: false,
        });
        if trigger.state & SHIFT_MASK == 0 {
            shortcuts.push(Shortcut {
                keyval  : trigger.keyval,
                state   : trigger.state | SHIFT_MASK,
                backward: true,
            });
        }
    }

    shortcuts
}

// --- Engine cycling ---

/// The next engine in the cycle, or `None` if there is nothing to switch to.
///
/// The list is fixed rather than most-recently-used. `ibus-ui-gtk3` keeps its
/// own list in MRU order with the current engine at index 0 and switches to
/// index 1 or `len - 1` (`ui/gtk3/panel.vala:1345-1352`); that ordering is
/// what makes its switcher popup useful, and with no popup it only makes the
/// cycle unpredictable. So the position of the current engine is found in the
/// configured order and the step is taken from there.
///
/// A current engine the list does not contain — the user switched by some
/// other route, or the list changed under us — lands on the first entry going
/// forward and the last going backward, which is the shortest way back into
/// the cycle in each direction.
pub fn next_engine<'a>(
    engines : &'a [String],
    current : Option<&str>,
    backward: bool,
) -> Option<&'a str> {
    if engines.is_empty() {
        return None;
    }

    let position = current.and_then(|current| {
        engines.iter().position(|engine| engine == current)
    });

    let index = match position {
        Some(_) if engines.len() < 2 => return None,
        Some(index) if backward      => (index + engines.len() - 1) % engines.len(),
        Some(index)                  => (index + 1) % engines.len(),
        None if backward             => engines.len() - 1,
        None                         => 0,
    };

    Some(engines[index].as_str())
}

// --- dconf ---

/// The dconf schema holding the trigger list.
const HOTKEY_SCHEMA: &str = "org.freedesktop.ibus.general.hotkey";

/// The dconf schema holding the engine list and its saved order.
const GENERAL_SCHEMA: &str = "org.freedesktop.ibus.general";

/// Reads a `as` key with `gsettings`.
///
/// Shelling out rather than linking `gio`: nothing in the dependency graph
/// pulls glib in today, and adding it to read three string lists at startup
/// would be the largest dependency in the tree by build time. The cost is that
/// a missing `gsettings` or an unregistered schema is indistinguishable from
/// an empty list, which is why both are logged.
fn gsettings_strv(schema: &str, key: &str) -> Vec<String> {
    let output = match std::process::Command::new("gsettings")
        .args(["get", schema, key])
        .output()
    {
        Ok(output) => output,
        Err(e)     => {
            tracing::warn!("could not run gsettings for {schema} {key}: {e}");
            return Vec::new();
        }
    };

    if !output.status.success() {
        tracing::warn!(
            "gsettings get {schema} {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Vec::new();
    }

    parse_gvariant_strv(&String::from_utf8_lossy(&output.stdout))
}

/// Parses GVariant's printed form of a string array.
///
/// `gsettings get` prints `['a', 'b']`, or `@as []` for an empty one that
/// needs its type spelling out. Only single-quoted strings appear — GVariant's
/// printer prefers them and escapes `'` and `\` inside — so the parser is a
/// scan for quoted runs rather than anything that deserves the word grammar.
fn parse_gvariant_strv(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '\'' {
            continue;
        }
        let mut value = String::new();
        loop {
            match characters.next() {
                Some('\'')  => break,
                Some('\\')  => match characters.next() {
                    Some(escaped) => value.push(escaped),
                    None          => break,
                },
                Some(other) => value.push(other),
                None        => break,
            }
        }
        values.push(value);
    }

    values
}

/// The engine-switch accelerators the user configured, from dconf.
pub fn dconf_triggers() -> Vec<String> {
    gsettings_strv(HOTKEY_SCHEMA, "triggers")
}

/// The engines to cycle through, from dconf, in the order the panel would use.
///
/// `preload-engines` is the set and `engines-order` is the remembered order;
/// the panel intersects them and appends whatever the order does not mention
/// (`ui/gtk3/panel.vala:1390-1408`), which is what this reproduces. The
/// daemon's own `ActiveEngines` is not an alternative: it is empty here, since
/// the daemon only fills it from loaded components (phase-1 finding 2).
pub fn dconf_engines() -> Vec<String> {
    let configured = gsettings_strv(GENERAL_SCHEMA, "preload-engines");
    let order = gsettings_strv(GENERAL_SCHEMA, "engines-order");

    let mut engines: Vec<String> = order
        .into_iter()
        .filter(|name| configured.contains(name))
        .collect();
    for name in configured {
        if !engines.contains(&name) {
            engines.push(name);
        }
    }

    engines
}

// --- The switcher ---

/// The panel connection, the trigger registration and the engine cycle.
///
/// Shaped like [`super::link::Link`] on purpose: `ensure` on a retry clock,
/// `lost` when the stream ends, and a thread parked in the signal stream
/// feeding the loop's channel. The two are separate objects because they own
/// separate connections — see [`crate::ibus::Panel`] for why the panel needs
/// one of its own.
pub struct Switcher {
    /// An address supplied by the caller, overriding discovery, so a harness
    /// can point this at a scratch daemon.
    override_address: Option<Address>,
    /// Accelerators from the config file. Empty means "read dconf".
    config_triggers : Vec<String>,
    /// Engine ids from the config file. Empty means "read dconf".
    config_engines  : Vec<String>,
    /// Where to publish engine changes, if anybody asked.
    events          : Option<mpsc::UnboundedSender<ImEvent>>,
    /// Whether this process is the daemon's panel. False when `ibus-ui-gtk3`
    /// already is, in which case nothing here runs at all — see
    /// [`Switcher::ensure`].
    enabled         : bool,

    /// The connection, while there is one.
    panel           : Option<Panel>,
    /// The engine cycle as resolved at the last connection. Re-resolved on
    /// each one, so editing dconf and restarting ibus-daemon is enough to pick
    /// up a new list.
    engines         : Vec<String>,
    /// Display names and symbols for the engines we have looked up, so a
    /// switch does not cost a registry round trip.
    described       : HashMap<String, (String, String)>,
    /// The engine the daemon last told us about.
    current         : Option<String>,
    /// When the next connection attempt may happen. `None` means "now".
    retry_at        : Option<Instant>,
    /// Whether the current run of failures has already been reported.
    reported        : bool,
}

impl Switcher {
    /// A switcher that has not connected yet.
    pub fn new(
        address : Option<String>,
        triggers: Vec<String>,
        engines : Vec<String>,
        events  : Option<mpsc::UnboundedSender<ImEvent>>,
        enabled : bool,
    ) -> Self {
        Self {
            override_address: address.map(Address::explicit),
            config_triggers : triggers,
            config_engines  : engines,
            events          : events,
            enabled         : enabled,
            panel           : None,
            engines         : Vec::new(),
            described       : HashMap::new(),
            current         : None,
            retry_at        : None,
            reported        : false,
        }
    }

    /// Connects and registers if that has not happened yet and enough time has
    /// passed since the last failure.
    ///
    /// Returns whether there is a registered trigger afterwards. Failures are
    /// logged once per run rather than propagated, for the same reason
    /// [`super::link::Link::ensure`] does it: IBus not running is a state the
    /// frontend works in, and here it costs only the switch hotkey.
    pub fn ensure(&mut self, signals: &Sender<Upstream>) -> bool {
        // Not merely "do not register": do not connect at all. Another panel's
        // `GlobalShortcutKeyResponded` is broadcast to everyone who subscribed
        // to it, so a listening-but-not-registering switcher would cycle the
        // engine a second time on every press of somebody else's hotkey.
        if !self.enabled {
            return false;
        }
        if self.panel.is_some() {
            return true;
        }
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        self.retry_at = Some(Instant::now() + RETRY_INTERVAL);

        match self.connect(signals) {
            Ok(())  => {
                self.reported = false;
                true
            }
            Err(e)  => {
                if self.reported {
                    tracing::debug!("ibus panel still unavailable: {e}");
                } else {
                    tracing::warn!("no engine-switch trigger registered: {e}");
                    self.reported = true;
                }
                self.panel = None;
                false
            }
        }
    }

    /// One attempt, from socket to a registered trigger and a signal thread.
    fn connect(&mut self, signals: &Sender<Upstream>) -> crate::ibus::Result<()> {
        let address = match &self.override_address {
            Some(address) => address.clone(),
            None          => Address::discover()?,
        };
        let mut panel = Panel::connect(address)?;

        let triggers = if self.config_triggers.is_empty() {
            dconf_triggers()
        } else {
            self.config_triggers.clone()
        };
        let shortcuts = switcher_shortcuts(&triggers);

        // The daemon refuses an empty list outright
        // (`bus/ibusimpl.c:2025`), and registering nothing would flip it into
        // "Wayland session" mode for no benefit if it did not — so skipping is
        // both the polite and the correct answer.
        if shortcuts.is_empty() {
            tracing::warn!(
                "no usable engine-switch trigger in {triggers:?}; the switch hotkey is off"
            );
        } else {
            panel.register(&shortcuts)?;
            tracing::info!(
                "engine-switch trigger registered: {}",
                describe_shortcuts(&shortcuts)
            );
        }

        self.engines = if self.config_engines.is_empty() {
            dconf_engines()
        } else {
            self.config_engines.clone()
        };
        self.describe(&panel);

        self.current = match panel.global_engine() {
            Ok(name) => Some(name),
            Err(e)   => {
                tracing::debug!("reading the global engine: {e}");
                None
            }
        };
        tracing::info!(
            "ibus panel on {}: engine cycle {:?}, currently {}",
            panel.address(),
            self.engines,
            self.current.as_deref().unwrap_or("unknown")
        );

        if let Some(stream) = panel.take_signals() {
            spawn_signal_thread(stream, signals.clone());
        }
        self.panel = Some(panel);

        self.choose_engine_if_none();

        Ok(())
    }

    /// Gives a daemon that has no global engine the head of the cycle.
    ///
    /// Choosing the engine at startup was ibus-ui-gtk3's job — `update_engines`
    /// ends in `switch_engine(0, true)` (`ui/gtk3/panel.vala:1445`) — and a
    /// daemon started with `--panel disable` has nobody else to do it. Until
    /// somebody does, `use-global-engine` means *no* engine: every key comes
    /// back unhandled, mozc never starts, and the popup reads "waiting for
    /// IBus" indefinitely (phase-5 finding 11).
    ///
    /// Only the empty case. ibus-ui-gtk3 also jumps to the head of its cycle
    /// when the daemon's engine is one it does not list; we keep that engine,
    /// because it got there by the user's hand through some other route, and
    /// [`next_engine`] already treats an outsider as "before the first".
    ///
    /// `current` is left for `GlobalEngineChanged` to fill in, as after any
    /// other switch: the confirmation is the source of truth, and the signal
    /// thread is already running to receive it.
    fn choose_engine_if_none(&self) {
        if self.current.is_some() {
            return;
        }
        let (Some(first), Some(panel)) = (self.engines.first(), self.panel.as_ref()) else {
            return;
        };
        match panel.set_global_engine(first) {
            Ok(())  => tracing::info!("ibus had no global engine; chose {first}"),
            Err(e)  => tracing::warn!("ibus has no global engine, and choosing {first} failed: {e}"),
        }
    }

    /// Fills the description cache and reports engines the registry lacks.
    ///
    /// A configured engine that is not installed is the failure that otherwise
    /// shows up as "the hotkey does nothing every other press", so it is worth
    /// one call and a warning.
    fn describe(&mut self, panel: &Panel) {
        if self.engines.is_empty() {
            tracing::warn!("no engines to cycle through; the switch hotkey will do nothing");
            return;
        }

        let names: Vec<&str> = self.engines.iter().map(String::as_str).collect();
        let described = match panel.describe_engines(&names) {
            Ok(described) => described,
            Err(e)        => {
                tracing::warn!("could not look the configured engines up: {e}");
                return;
            }
        };

        self.described.clear();
        for engine in &described {
            self.described.insert(
                engine.name.clone(),
                (engine.symbol.clone(), engine.longname.clone()),
            );
        }
        for name in &self.engines {
            if !self.described.contains_key(name) {
                tracing::warn!("engine {name:?} is configured but not in the ibus registry");
            }
        }
    }

    /// Acts on one signal from the daemon's own object.
    pub fn on_signal(&mut self, signal: PanelSignal) {
        match signal {
            PanelSignal::ShortcutKeyResponded { binding, state, backward, .. } => {
                self.on_trigger(binding, state, backward);
            }
            PanelSignal::GlobalEngineChanged { name } => self.on_engine_changed(name),
            // The registry was rebuilt, so the cached symbols and long names
            // may be for engines that no longer exist. Cheap to drop; the next
            // change refills what it needs.
            PanelSignal::RegistryChanged => {
                tracing::debug!("ibus registry changed; dropping cached engine descriptions");
                self.described.clear();
            }
        }
    }

    /// The switch trigger fired.
    fn on_trigger(&mut self, binding: u8, state: u32, backward: bool) {
        if binding != BINDING_TYPE_IME_SWITCHER {
            tracing::debug!("ignoring global shortcut of type {binding}");
            return;
        }
        // The release edge is the daemon's other half of the switcher-popup
        // protocol. With no popup there is nothing to commit, and acting on
        // both edges would cycle twice per press.
        if state & RELEASE_MASK != 0 {
            tracing::debug!("engine-switch trigger released; nothing to commit");
            return;
        }

        let Some(next) = next_engine(&self.engines, self.current.as_deref(), backward) else {
            tracing::info!(
                "engine-switch trigger fired with nothing to switch to (cycle {:?})",
                self.engines
            );
            return;
        };
        let next = next.to_string();

        let Some(panel) = self.panel.as_ref() else {
            return;
        };
        tracing::info!(
            "engine-switch trigger: {} -> {next} ({})",
            self.current.as_deref().unwrap_or("unknown"),
            if backward { "backward" } else { "forward" }
        );
        if let Err(e) = panel.set_global_engine(&next) {
            tracing::warn!("could not switch to {next}: {e}");
        }
        // `current` is deliberately not updated here. The daemon answers with
        // `GlobalEngineChanged` whether the switch came from us or from
        // anywhere else, and taking the confirmation as the source of truth is
        // what keeps the cycle from drifting when a switch is refused.
    }

    /// The global engine changed, ours or anybody's.
    fn on_engine_changed(&mut self, name: String) {
        self.current = Some(name.clone());

        if !self.described.contains_key(&name)
            && let Some(panel) = self.panel.as_ref()
        {
            match panel.describe_engine(&name) {
                Ok(Some(engine)) => {
                    self.described
                        .insert(name.clone(), (engine.symbol, engine.longname));
                }
                Ok(None) => tracing::debug!("the registry does not know engine {name:?}"),
                Err(e)   => tracing::debug!("describing engine {name:?}: {e}"),
            }
        }

        let (symbol, longname) = self
            .described
            .get(&name)
            .cloned()
            .unwrap_or_else(|| (String::new(), name.clone()));
        let event = ImEvent::EngineChanged {
            name    : name,
            symbol  : symbol,
            longname: longname,
        };

        tracing::info!("ibus {event}");

        if let Some(events) = self.events.as_ref() {
            // A closed receiver means whoever wanted these has gone; the log
            // line above is still the record.
            if events.send(event).is_err() {
                self.events = None;
            }
        }
    }

    /// Tears the panel connection down after the daemon went away.
    ///
    /// The registration dies with the daemon, so the next successful `ensure`
    /// has to make it again — which is the whole reason this is on the same
    /// retry clock as the context rather than done once at startup.
    pub fn lost(&mut self, reason: &str) {
        if self.panel.is_some() {
            tracing::warn!("ibus panel connection lost ({reason}); no engine-switch trigger");
        }
        self.panel = None;
        self.current = None;
        self.retry_at = Some(Instant::now() + RETRY_INTERVAL);
    }
}

/// Renders a registration for the log, so the trace shows what the daemon will
/// be comparing against rather than what the config said.
fn describe_shortcuts(shortcuts: &[Shortcut]) -> String {
    shortcuts
        .iter()
        .map(|shortcut| {
            format!(
                "{}{}{}",
                crate::ibus::describe_state(shortcut.state),
                if shortcut.state == 0 { "" } else { "+" },
                xkb::keysym_get_name(xkb::Keysym::new(shortcut.keyval)),
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Starts the thread that blocks on the panel's signal stream.
///
/// Separate from [`super::link`]'s thread because it is a separate connection;
/// both end the same way, by the stream running out when ibus-daemon dies.
fn spawn_signal_thread(mut stream: PanelStream, signals: Sender<Upstream>) {
    let spawned = std::thread::Builder::new()
        .name("cosmic-voice-panel".to_string())
        .spawn(move || {
            loop {
                match stream.next(None) {
                    Ok(Some(signal)) => {
                        if signals.send(Upstream::Panel(signal)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = signals.send(Upstream::PanelLost);
                        break;
                    }
                    Err(e) => {
                        tracing::debug!("ibus panel signal stream ended: {e}");
                        let _ = signals.send(Upstream::PanelLost);
                        break;
                    }
                }
            }
        });

    if let Err(e) = spawned {
        tracing::error!("could not start the ibus panel signal thread: {e}");
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The value in this machine's dconf, and the one the harness registers.
    #[test]
    fn parses_control_alt_space() {
        let trigger = parse_accelerator("<Control><Alt>space").expect("parse");
        assert_eq!(trigger.keyval, xkb::keysyms::KEY_space);
        assert_eq!(trigger.state, CONTROL_MASK | MOD1_MASK);
    }

    /// The schema default, and the one the harness's own empty dconf yields.
    /// `<Super>` must come out as MOD4 rather than `IBUS_SUPER_MASK`, which
    /// `IBUS_MODIFIER_FILTER` would strip before the daemon compared.
    #[test]
    fn parses_super_space_as_mod4() {
        let trigger = parse_accelerator("<Super>space").expect("parse");
        assert_eq!(trigger.keyval, xkb::keysyms::KEY_space);
        assert_eq!(trigger.state, MOD4_MASK);
        assert_eq!(trigger.state & MODIFIER_FILTER, trigger.state);
    }

    /// A non-Latin keysym with two modifiers, to prove the key name goes
    /// through xkbcommon rather than a table of the ones we thought of.
    #[test]
    fn parses_shift_control_kanji() {
        let trigger = parse_accelerator("<Shift><Control>Kanji").expect("parse");
        assert_eq!(trigger.keyval, xkb::keysyms::KEY_Kanji);
        assert_eq!(trigger.state, SHIFT_MASK | CONTROL_MASK);
    }

    /// Nothing should turn into a shortcut that matches every key or none.
    #[test]
    fn rejects_nonsense() {
        assert!(parse_accelerator("").is_none());
        assert!(parse_accelerator("<Control>").is_none());
        assert!(parse_accelerator("<Nonsuch>space").is_none());
        assert!(parse_accelerator("<Control><Alt>not_a_keysym").is_none());
        assert!(parse_accelerator("<Controlspace").is_none());
    }

    /// One accelerator, two registrations, and the backward one is the
    /// Shift-modified twin.
    #[test]
    fn registers_a_backward_twin() {
        let shortcuts = switcher_shortcuts(&["<Control><Alt>space".to_string()]);
        assert_eq!(shortcuts.len(), 2);
        assert_eq!(shortcuts[0].state, CONTROL_MASK | MOD1_MASK);
        assert!(!shortcuts[0].backward);
        assert_eq!(shortcuts[1].state, CONTROL_MASK | MOD1_MASK | SHIFT_MASK);
        assert!(shortcuts[1].backward);
        assert_eq!(shortcuts[0].keyval, shortcuts[1].keyval);
    }

    /// An accelerator that already carries Shift gets no twin: the twin would
    /// be the same combination registered twice with opposite directions, and
    /// the daemon takes the first match.
    #[test]
    fn shift_accelerator_has_no_twin() {
        let shortcuts = switcher_shortcuts(&["<Shift><Super>space".to_string()]);
        assert_eq!(shortcuts.len(), 1);
        assert!(!shortcuts[0].backward);
    }

    /// A bad entry costs itself and nothing else.
    #[test]
    fn a_bad_accelerator_does_not_take_the_good_ones_with_it() {
        let shortcuts = switcher_shortcuts(&[
            "<Nonsuch>space".to_string(),
            "<Control><Alt>space".to_string(),
        ]);
        assert_eq!(shortcuts.len(), 2);
    }

    /// The three-engine case, which is the only one where forward and backward
    /// are distinguishable — the harness runs two engines, where they are not.
    #[test]
    fn cycles_both_ways() {
        let engines: Vec<String> = ["xkb:us::eng", "mozc-on", "mozc-jp"]
            .iter()
            .map(|name| name.to_string())
            .collect();

        assert_eq!(next_engine(&engines, Some("xkb:us::eng"), false), Some("mozc-on"));
        assert_eq!(next_engine(&engines, Some("mozc-on"), false), Some("mozc-jp"));
        assert_eq!(next_engine(&engines, Some("mozc-jp"), false), Some("xkb:us::eng"));

        assert_eq!(next_engine(&engines, Some("xkb:us::eng"), true), Some("mozc-jp"));
        assert_eq!(next_engine(&engines, Some("mozc-jp"), true), Some("mozc-on"));
        assert_eq!(next_engine(&engines, Some("mozc-on"), true), Some("xkb:us::eng"));
    }

    /// Two engines toggle, which is what the harness asserts against a real
    /// daemon.
    #[test]
    fn two_engines_toggle() {
        let engines: Vec<String> = ["xkb:us::eng", "mozc-on"]
            .iter()
            .map(|name| name.to_string())
            .collect();

        assert_eq!(next_engine(&engines, Some("xkb:us::eng"), false), Some("mozc-on"));
        assert_eq!(next_engine(&engines, Some("mozc-on"), false), Some("xkb:us::eng"));
        assert_eq!(next_engine(&engines, Some("xkb:us::eng"), true), Some("mozc-on"));
    }

    /// An engine outside the cycle, and the degenerate lists.
    #[test]
    fn unknown_and_degenerate_currents() {
        let engines: Vec<String> = ["xkb:us::eng", "mozc-on", "mozc-jp"]
            .iter()
            .map(|name| name.to_string())
            .collect();

        assert_eq!(next_engine(&engines, Some("anthy"), false), Some("xkb:us::eng"));
        assert_eq!(next_engine(&engines, Some("anthy"), true), Some("mozc-jp"));
        assert_eq!(next_engine(&engines, None, false), Some("xkb:us::eng"));
        assert_eq!(next_engine(&engines, None, true), Some("mozc-jp"));

        assert_eq!(next_engine(&[], Some("mozc-on"), false), None);
        let one = vec!["mozc-on".to_string()];
        assert_eq!(next_engine(&one, Some("mozc-on"), false), None);
        assert_eq!(next_engine(&one, Some("anthy"), false), Some("mozc-on"));
    }

    /// The two shapes `gsettings get` prints for an `as`.
    #[test]
    fn parses_gvariant_string_arrays() {
        assert_eq!(
            parse_gvariant_strv("['xkb:us::eng', 'mozc-jp']\n"),
            vec!["xkb:us::eng".to_string(), "mozc-jp".to_string()]
        );
        assert_eq!(parse_gvariant_strv("@as []\n"), Vec::<String>::new());
        assert_eq!(
            parse_gvariant_strv(r"['it\'s', 'a\\b']"),
            vec!["it's".to_string(), r"a\b".to_string()]
        );
    }
}
