//! The frontend's end of the connection to ibus-daemon.
//!
//! [`crate::ibus`] knows how to talk to the daemon; this knows *when*. It owns
//! the one input context the frontend uses, decides when to build it and when
//! to give up on it, and arranges for the daemon's asynchronous signals to
//! arrive as events in the calloop loop rather than as something anybody has
//! to poll for.
//!
//! # Why the signals need a thread
//!
//! A context has two halves that both want to block. The calloop loop calls
//! [`crate::ibus::Context::process_key`] inline and waits for the engine's
//! answer, because the whole value of the synchronous key path is that the
//! answer and its effects arrive together. Meanwhile something has to be
//! sitting in the signal stream, because zbus buffers a bounded number of
//! messages per stream and stalls the connection when the buffer fills — a
//! context whose signals nobody reads eventually wedges the client.
//!
//! One `&mut Context` cannot be in both places, so the stream detaches
//! ([`crate::ibus::Context::take_signals`]) and moves to a thread whose only
//! job is `next(None)` in a loop, forwarding into a `calloop::channel`. The
//! thread ends when the stream does, which is also how the frontend learns the
//! daemon died: [`Upstream::Lost`] arrives, the context is dropped, and keys
//! start passing through raw until a retry succeeds.
//!
//! # Reconnection
//!
//! By retry timer rather than by watching `org.freedesktop.IBus` on the
//! session bus. The retry *is* the watch: [`crate::ibus::Address::discover`]
//! re-reads the address file and validates the daemon's PID, so an attempt
//! every few seconds detects a restarted daemon and picks up its new socket in
//! one step. A name watch would tell us the name came back but not what
//! address it came back on, so we would end up doing this anyway.

use std::time::{Duration, Instant};

use calloop::channel::Sender;

use crate::ibus::{
    Address, Bus, CAPABILITIES, CLIENT_NAME, Context, ContextSignal, SignalStream,
    describe_capabilities,
};

/// How long to wait before trying a daemon that was not there again.
///
/// Three seconds is a compromise between noticing a restarted ibus-daemon
/// while the user is still looking at the same text field, and not doing
/// filesystem work every second forever on a machine where IBus is simply not
/// installed.
const RETRY_INTERVAL: Duration = Duration::from_secs(3);

// --- Events out ---

/// What the signal threads deliver into the loop.
///
/// Two connections feed this: the input context's, and the panel's
/// ([`super::switcher`]). They are separate variants rather than separate
/// channels because the loop has one channel source and the two connections
/// fail for the same reason at the same moment.
#[derive(Debug)]
pub enum Upstream {
    /// The daemon sent something outside any key's drain.
    Signal(ContextSignal),
    /// The connection ended. Everything built on it is gone.
    Lost,
    /// The daemon's own object announced something: a fired shortcut, an
    /// engine change, a rebuilt registry.
    Panel(crate::ibus::PanelSignal),
    /// The panel connection ended, so the trigger registration is gone with
    /// the daemon that held it.
    PanelLost,
}

// --- The link ---

/// The one input context the frontend uses, and its lifecycle.
pub struct Link {
    /// An address supplied by the caller, which overrides discovery. Kept as
    /// an [`Address`] rather than a string so that a reconnect uses the same
    /// override rather than silently falling back to the session's daemon.
    override_address: Option<Address>,
    /// The connection, while there is one.
    bus             : Option<Bus>,
    /// The context, while there is one. Created lazily on first activation:
    /// creating it is what makes the daemon start an engine for us, and doing
    /// that before a text field has focus would take the engine away from
    /// whatever the user is really typing in.
    context         : Option<Context>,
    /// When the next connection attempt may happen. `None` means "now".
    retry_at        : Option<Instant>,
    /// Whether the current run of failures has already been reported. IBus
    /// simply not being installed is a supported state, and a warning every
    /// three seconds forever would bury the log lines that matter.
    reported        : bool,
}

impl Link {
    /// A link that has not connected yet.
    pub fn new(address: Option<String>) -> Self {
        Self {
            override_address: address.map(Address::explicit),
            bus             : None,
            context         : None,
            retry_at        : None,
            reported        : false,
        }
    }

    /// The context, if there is one right now.
    pub fn context(&self) -> Option<&Context> {
        self.context.as_ref()
    }

    /// Builds the context if it is missing and enough time has passed since
    /// the last failure.
    ///
    /// Returns whether there is a usable context afterwards. Failures are
    /// logged once per attempt rather than propagated: "IBus is not running"
    /// is a state the frontend is designed to work in, not an error — keys
    /// pass through raw and English still types.
    pub fn ensure(&mut self, signals: &Sender<Upstream>) -> bool {
        if self.context.is_some() {
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
                    tracing::debug!("ibus still unavailable: {e}");
                } else {
                    tracing::warn!("ibus unavailable, passing keys through: {e}");
                    self.reported = true;
                }
                self.bus = None;
                self.context = None;
                false
            }
        }
    }

    /// One connection attempt, from socket to a context with a signal thread.
    fn connect(&mut self, signals: &Sender<Upstream>) -> crate::ibus::Result<()> {
        let address = match &self.override_address {
            Some(address) => address.clone(),
            None          => Address::discover()?,
        };
        let bus = Bus::connect_to(address)?;
        tracing::info!(
            "ibus connected: {} as {}",
            bus.address().source,
            bus.unique_name()
        );

        // Phase-1 finding 7: the daemon only routes preedit to a client when
        // `EmbedPreeditText` is on, and any client can turn it off. With it off
        // our preedit would be sent to a panel process that no longer exists,
        // and the symptom is silent — typing works, nothing shows.
        match bus.embed_preedit_text() {
            Ok(true)  => {}
            Ok(false) => tracing::warn!(
                "ibus EmbedPreeditText is off: engines will send preedit to a panel, not to us"
            ),
            Err(e)    => tracing::warn!("could not read ibus EmbedPreeditText: {e}"),
        }

        let mut context = bus.create_input_context(CLIENT_NAME)?;
        tracing::info!(
            "ibus context {} with {}",
            context.path(),
            describe_capabilities(CAPABILITIES)
        );

        if let Some(stream) = context.take_signals() {
            spawn_signal_thread(stream, signals.clone());
        }

        self.bus = Some(bus);
        self.context = Some(context);

        Ok(())
    }

    /// Tears the context down after the daemon went away.
    ///
    /// The retry clock starts here, so a daemon that is crash-looping does not
    /// turn every keystroke into a connection attempt.
    pub fn lost(&mut self, reason: &str) {
        if self.context.is_some() || self.bus.is_some() {
            tracing::warn!("ibus connection lost ({reason}); keys pass through until it returns");
        }
        self.context = None;
        self.bus = None;
        self.retry_at = Some(Instant::now() + RETRY_INTERVAL);
    }

    /// Whether an error from the daemon means the connection is dead rather
    /// than the call being wrong.
    ///
    /// Method errors are ordinary — the daemon refuses `SetEngine` under
    /// `use-global-engine`, for instance — and must not tear anything down.
    /// A transport error is different: nothing on this connection will ever
    /// work again.
    pub fn is_fatal(error: &crate::ibus::Error) -> bool {
        matches!(
            error,
            crate::ibus::Error::Dbus(
                zbus::Error::InputOutput(_) | zbus::Error::Failure(_) | zbus::Error::Unsupported
            )
        )
    }

    /// Reports the engine the daemon has in effect, for the log line that
    /// tells the user which one their keys are going to.
    ///
    /// With `use-global-engine` on there is no per-context engine to ask about
    /// and no `SetEngine` to call (phase-1 finding 9), so this is a read and
    /// nothing more: engine choice belongs to the daemon and, in phase 4, to
    /// the panel duties that switch it globally.
    pub fn global_engine(&self) -> Option<String> {
        let bus = self.bus.as_ref()?;
        match bus.global_engine() {
            Ok(engine) => Some(engine.name),
            Err(e)     => {
                tracing::debug!("reading the global engine: {e}");
                None
            }
        }
    }
}

/// Starts the thread that blocks on the signal stream.
///
/// Named so it is identifiable in a backtrace; a thread that is permanently
/// blocked in a D-Bus read is exactly the sort of thing that shows up in one.
fn spawn_signal_thread(mut stream: SignalStream, signals: Sender<Upstream>) {
    let spawned = std::thread::Builder::new()
        .name("cosmic-voice-ibus".to_string())
        .spawn(move || {
            loop {
                match stream.next(None) {
                    Ok(Some(signal)) => {
                        // A send failure means the loop is gone, which means
                        // the process is shutting down.
                        if signals.send(Upstream::Signal(signal)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = signals.send(Upstream::Lost);
                        break;
                    }
                    Err(e) => {
                        tracing::debug!("ibus signal stream ended: {e}");
                        let _ = signals.send(Upstream::Lost);
                        break;
                    }
                }
            }
        });

    if let Err(e) = spawned {
        tracing::error!("could not start the ibus signal thread: {e}");
    }
}
