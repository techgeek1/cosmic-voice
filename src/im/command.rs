//! The engine's road into the frontend, and the one fact travelling back.
//!
//! [`super::dictation`] decides what a dictation command *does*; this is the
//! plumbing that gets any command from the rest of the process onto the
//! frontend's calloop loop without either side being able to block the other.
//! Three things travel it today: the dictation turn-taking commands, an engine
//! switch requested from the popup or the CLI, and a status-menu activation
//! from the popup. They share one channel because they share one destination
//! and one failure mode — a frontend that is not running drops all of them.
//!
//! # Why the answer is an atomic and not a reply
//!
//! The dictation engine has to choose between the input-method path and the
//! virtual keyboard at the moment it wants to show or commit text, and it
//! must not wait for the answer: its loop also drives audio capture. A
//! request/reply over a channel would either block it or hand it an answer
//! that was already stale by the time it arrived. So the frontend *publishes*
//! the fact instead — [`ImStatus`] is written on every activation change and
//! read whenever the engine is about to speak — and the residual race (the
//! field goes away in the microseconds between the read and the send) is
//! resolved by [`super::dictation::advance`] on the other side, which sees the
//! current value.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use calloop::channel::Sender;
use serde::{Deserialize, Serialize};

use super::dictation::DictationCmd;

// --- The commands ---

/// Everything the rest of the process can ask the frontend to do.
///
/// Serialisable because the harness feeds these in from a shell script through
/// a fifo (`devtest im-frontend --control-fifo`), which is what lets
/// turn-taking and the status menu be tested end to end against a real mozc
/// with no microphone and no panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImCmd {
    /// A dictation turn-taking command. See [`super::dictation`].
    Dictation(DictationCmd),
    /// Switch the global engine, by id. Goes to the switcher, which asks the
    /// daemon and waits for `GlobalEngineChanged` like it does for the hotkey.
    SetEngine(String),
    /// Activate one status-menu entry. Goes to the input context, whose
    /// engine answers with `UpdateProperty` signals.
    ActivateProperty {
        /// The property's key.
        key  : String,
        /// The `PROP_STATE_*` value the entry should take.
        state: u32,
    },
}

// --- The status flags ---

/// What the frontend publishes about itself, for the engine to read.
///
/// Two flags rather than one because they fail differently: `bound` says a
/// frontend exists at all (it is false while the supervisor is backing off
/// after a crash, and while the input method is blocked), and `active` says a
/// text-input client has focus. The engine needs both to be true before the
/// input-method path is worth anything.
#[derive(Debug, Default)]
pub struct ImStatus {
    /// Whether a frontend is running and holds the input-method slot.
    bound : AtomicBool,
    /// Whether a text-input client is focused right now.
    active: AtomicBool,
}

impl ImStatus {
    /// Whether dictation can go through the input method at this instant.
    ///
    /// `Relaxed` throughout: there is no other memory being published
    /// alongside these flags, the writer is one thread and the reader is one
    /// thread, and a read that is one activation out of date is resolved by
    /// [`super::dictation::advance`] at the other end.
    pub fn is_active(&self) -> bool {
        self.bound.load(Ordering::Relaxed) && self.active.load(Ordering::Relaxed)
    }

    /// Records whether a frontend holds the slot. Called by the frontend on
    /// its way in and out.
    pub fn set_bound(&self, bound: bool) {
        self.bound.store(bound, Ordering::Relaxed);
        if !bound {
            self.active.store(false, Ordering::Relaxed);
        }
    }

    /// Records whether a text field has focus. Called by the frontend on every
    /// applied activation change.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

// --- The link ---

/// The process's end of the connection to the input-method thread.
///
/// Cloneable and outlives any one frontend: the sender inside is *replaced*
/// each time the supervisor starts a frontend, because a calloop channel
/// belongs to the loop that polls it and a restarted loop is a new one. The
/// engine holds this from process start and never learns that a restart
/// happened — its sends go nowhere while `commands` is empty, which is exactly
/// what "the input method is not available" already means to it.
#[derive(Debug, Clone, Default)]
pub struct ImLink {
    /// The running frontend's command channel, if one is running. The mutex is
    /// uncontended except at the instant of a restart; the alternative, a
    /// permanent channel plus a thread forwarding into a per-loop one, buys
    /// nothing but a thread.
    commands: Arc<Mutex<Option<Sender<ImCmd>>>>,
    /// What the frontend says about itself.
    status  : Arc<ImStatus>,
}

impl ImLink {
    /// A link with no frontend behind it yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The status flags, for the frontend to write.
    pub fn status(&self) -> &Arc<ImStatus> {
        &self.status
    }

    /// Whether dictation should use the input-method path right now.
    pub fn is_active(&self) -> bool {
        self.status.is_active()
    }

    /// Points the link at a freshly started frontend's channel.
    pub fn attach(&self, sender: Sender<ImCmd>) {
        *self.commands.lock().expect("the im link mutex is never poisoned") = Some(sender);
    }

    /// Forgets the frontend's channel, after it stopped.
    pub fn detach(&self) {
        *self.commands.lock().expect("the im link mutex is never poisoned") = None;
    }

    /// Sends one command, dropping it if no frontend is running.
    ///
    /// Never blocks and never fails upwards: a calloop channel is unbounded,
    /// and a send that fails means the loop it belonged to is gone, which the
    /// engine already handles by falling back to the virtual keyboard.
    pub fn send(&self, command: ImCmd) {
        let mut slot = self.commands.lock().expect("the im link mutex is never poisoned");
        let Some(sender) = slot.as_ref() else {
            tracing::debug!("dropping {command:?}: no input-method frontend is running");
            return;
        };
        if sender.send(command).is_err() {
            tracing::debug!("the input-method frontend stopped reading commands");
            *slot = None;
        }
    }

    /// Sends one dictation command. The common case, spelled short.
    pub fn dictate(&self, command: DictationCmd) {
        self.send(ImCmd::Dictation(command));
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The status flags are two conditions, not one: a frontend that is
    /// backing off after a crash is not usable even if the last thing it saw
    /// was an active field.
    #[test]
    fn status_needs_both_flags() {
        let status = ImStatus::default();
        assert!(!status.is_active());
        status.set_bound(true);
        assert!(!status.is_active());
        status.set_active(true);
        assert!(status.is_active());
        // Losing the frontend clears the activation with it, so a restarted
        // one cannot inherit a stale `true`.
        status.set_bound(false);
        status.set_bound(true);
        assert!(!status.is_active());
    }

    /// The harness writes these by hand, so the spelling is part of the
    /// interface: a bare dictation command still parses through the wrapper
    /// the fifo reader tries second, and the two new ones read as written in
    /// `property-test.sh`.
    #[test]
    fn commands_parse_as_the_harness_writes_them() {
        let parsed: ImCmd = serde_json::from_str(r#"{"Dictation":"Begin"}"#).expect("dictation");
        assert_eq!(parsed, ImCmd::Dictation(DictationCmd::Begin));

        let parsed: ImCmd = serde_json::from_str(r#"{"SetEngine":"xkb:us::eng"}"#).expect("engine");
        assert_eq!(parsed, ImCmd::SetEngine("xkb:us::eng".to_string()));

        let parsed: ImCmd =
            serde_json::from_str(r#"{"ActivateProperty":{"key":"InputMode.Direct","state":1}}"#)
                .expect("property");
        assert_eq!(
            parsed,
            ImCmd::ActivateProperty {
                key  : "InputMode.Direct".to_string(),
                state: 1,
            }
        );
    }
}
