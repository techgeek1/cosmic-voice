//! Keeping an input-method frontend running inside the applet process.
//!
//! [`super::frontend::run`] is a blocking call that owns a thread for as long
//! as it holds the seat's input-method slot. The design doc says to run it
//! "under a supervisor (systemd user unit)"; inside the applet that becomes a
//! thread and this loop, for the same reason and with the same contract —
//! whatever kills the frontend must not take the panel with it, and the user
//! must be able to see that it happened.
//!
//! # What the frontend dying costs
//!
//! Nothing, as far as the keyboard is concerned. The grab and the input method
//! die with the connection, and a grab object's destructor is what releases
//! the seat-wide grab, so key flow is restored by the crash itself — that is
//! the difference between us and IBus's bridge in the 2026-08 incident
//! (`docs/multiplexer.md`). What is lost is the input method: until a new
//! frontend binds, typing is raw and dictation falls back to the virtual
//! keyboard, which is exactly the state the applet shows.
//!
//! # Why the restart backs off
//!
//! The two ways to fail are transient (the compositor restarted, a global was
//! missing for an instant) and permanent (`zwp_input_method_manager_v2` is not
//! advertised at all). A fixed retry turns the second into a busy loop that
//! binds and unbinds an exclusive seat resource forever, which is precisely
//! the sort of churn the protocol has no way to arbitrate. So the delay
//! doubles up to half a minute, and resets only after a run that lasted long
//! enough to have been working.
//!
//! # The check that is not the frontend's
//!
//! [`super::frontend::run`] refuses to bind while `ibus-ui-gtk3
//! --enable-wayland-im` holds the slot, and that refusal is the load-bearing
//! one. The supervisor makes the *same* check first, before calling it, for a
//! different purpose: a refusal from inside `run` is an error string, and this
//! state is not an error — it is the pre-cutover configuration, it is what
//! every user will see the first time they set `input_method: Multiplexer`,
//! and it deserves a sentence in the popup telling them what to do rather than
//! a stack of `Failed` events.

use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use super::ImEvent;
use super::dictation::DictationLink;

/// Delay before the first restart attempt.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling on the restart delay.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long a run must last to count as healthy, resetting the backoff.
///
/// A frontend that binds, works for a minute and then dies has hit something
/// transient; one that dies in five seconds is failing on startup, and the
/// next attempt will fail the same way.
const HEALTHY: Duration = Duration::from_secs(60);

/// How often to re-check whether IBus's bridge has gone away.
///
/// The cutover is `ibus exit; ibus start`, which the user runs by hand in a
/// terminal, so this is how long they wait before the applet notices. Short
/// enough not to be mistaken for "it did not work", long enough not to be
/// scanning `/proc` in a loop.
const BLOCKED_INTERVAL: Duration = Duration::from_secs(5);

// --- Options ---

/// Everything one frontend needs, kept so each attempt can rebuild it.
///
/// A struct rather than arguments because it is rebuilt per attempt:
/// [`super::Options`] is consumed by `run`, and the engine-switch triggers and
/// the engine cycle are `Vec`s that a restart has to be able to hand over
/// again.
pub struct Supervised {
    /// The Wayland display to bind on. This is the *live* one — see [`spawn`].
    pub display  : String,
    /// Engine-switch accelerators in GTK syntax, or empty for dconf's.
    pub triggers : Vec<String>,
    /// Engine ids to cycle through, or empty for dconf's.
    pub engines  : Vec<String>,
    /// The dictation engine's end of the turn-taking channel.
    pub dictation: DictationLink,
    /// Where the applet's state line comes from.
    pub events   : UnboundedSender<ImEvent>,
}

// --- The supervisor ---

/// Starts the input-method thread and returns immediately.
///
/// **This is the call that binds the live seat's input-method slot**, and it
/// is reached only from `input_method: Multiplexer` in the config, only in the
/// process holding the primary lock, and only past the check below. The
/// frontend's own live-display guard is deliberately overridden here
/// (`allow_live`), because owning the live seat's slot is the entire purpose
/// of the mode: the guard exists so that a *devtest* cannot reach this state
/// by accident, not to stop the shipped applet from doing its job.
pub fn spawn(supervised: Supervised) {
    let spawned = std::thread::Builder::new()
        .name("cosmic-voice-im".to_string())
        .spawn(move || run_supervised(supervised));

    if let Err(e) = spawned {
        tracing::error!("could not start the input-method thread: {e}");
    }
}

/// The supervision loop. Runs for the life of the process.
fn run_supervised(supervised: Supervised) {
    let mut backoff = FIRST_BACKOFF;
    let mut blocked_by = None;

    loop {
        // The pre-cutover state, and the one the user has to act on. Reported
        // once per change of pid so that five seconds of polling does not
        // become five seconds of events.
        if let Some(pid) = super::frontend::ibus_wayland_bridge() {
            if blocked_by != Some(pid) {
                blocked_by = Some(pid);
                report(
                    &supervised.events,
                    ImEvent::Blocked {
                        reason: format!(
                            "ibus-ui-gtk3 --enable-wayland-im is running (pid {pid}); \
                             run scripts/cutover.sh and restart IBus",
                        ),
                    },
                );
            }
            std::thread::sleep(BLOCKED_INTERVAL);
            continue;
        }
        blocked_by = None;

        let started = Instant::now();
        let outcome = super::run(super::Options {
            display     : supervised.display.clone(),
            ibus_address: None,
            allow_live  : true,
            triggers    : supervised.triggers.clone(),
            engines     : supervised.engines.clone(),
            events      : Some(supervised.events.clone()),
            dictation   : Some(supervised.dictation.clone()),
        });

        let reason = match outcome {
            Ok(())  => "the event loop exited".to_string(),
            Err(e)  => format!("{e:#}"),
        };
        tracing::warn!("input-method frontend stopped: {reason}");
        report(&supervised.events, ImEvent::Stopped { reason: reason });

        // A run that lasted is evidence the configuration works, so the next
        // failure starts its own backoff from scratch rather than inheriting
        // one from an incident half an hour ago.
        if started.elapsed() >= HEALTHY {
            backoff = FIRST_BACKOFF;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Publishes one event, giving up quietly when nobody is listening.
///
/// A closed receiver means the engine is gone, which means the process is
/// shutting down; the frontend logs everything it publishes anyway, so there
/// is nothing to report about a report.
fn report(events: &UnboundedSender<ImEvent>, event: ImEvent) {
    tracing::info!("im supervisor: {event}");
    let _ = events.send(event);
}
