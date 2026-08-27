//! Turn-taking between typing and dictation, and the channel that carries it.
//!
//! The multiplexer exists because a seat has one `zwp_input_method_v2` slot
//! and two things want it: IBus, driving a conversion from the keyboard, and
//! the dictation engine, driving one from a microphone. Only one human is
//! producing input at a time, so the contention is not real — it just has
//! nowhere to be resolved as long as the two live in different processes. Here
//! it is one enum and one function.
//!
//! # What is in this file, and why it is separate
//!
//! [`advance`] is the whole decision: a state, a command and one fact about
//! the world (is a text field focused right now?) in, the next state and the
//! protocol operations to perform out. Nothing in it touches Wayland, D-Bus,
//! the clock or a channel, for the same reason [`super::router`] does not —
//! the rest of the frontend needs a nested compositor and an attended session
//! to exercise, and this is where the interesting mistakes live. The tests at
//! the bottom of this file are the phase-5 regression that runs on `cargo
//! test`.
//!
//! The rest is the plumbing that gets a [`DictationCmd`] from the engine's
//! tokio loop onto the frontend's calloop loop without either being able to
//! block the other, and the one fact travelling the other way.
//!
//! # Why the answer is an atomic and not a reply
//!
//! The engine has to choose between the input-method path and the virtual
//! keyboard at the moment it wants to show or commit text, and it must not
//! wait for the answer: its loop also drives audio capture. A request/reply
//! over a channel would either block it or hand it an answer that was already
//! stale by the time it arrived. So the frontend *publishes* the fact instead
//! — [`ImStatus`] is written on every activation change and read whenever the
//! engine is about to speak — and the residual race (the field goes away in
//! the microseconds between the read and the send) is resolved by
//! [`advance`] on the other side, which sees the current value.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use calloop::channel::Sender;
use serde::{Deserialize, Serialize};

// --- The commands ---

/// What the dictation engine asks the input method to do.
///
/// Serialisable because the harness feeds these in from a shell script through
/// a fifo (`devtest im-frontend --dictation-fifo`), which is what lets the
/// turn-taking be tested end to end against a real mozc without a microphone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DictationCmd {
    /// An utterance is starting. Whatever the keyboard was in the middle of
    /// ends here.
    Begin,
    /// Provisional text from the streaming recogniser, replacing whatever the
    /// last one showed.
    Partial(String),
    /// The final transcript. Ends the utterance.
    Commit(String),
    /// The utterance was abandoned. Nothing is committed.
    Cancel,
}

// --- The state machine ---

/// Which input the router is carrying right now.
///
/// One per activation in the design; in practice one per process, because a
/// deactivation while dictating is not a reason to throw an utterance away —
/// the engine notices the field is gone from [`ImStatus`] and finishes on the
/// virtual keyboard instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Turn {
    /// Keys go to IBus, its preedit and commits go to the application. The
    /// steady state, and everything phases 2 to 4 built.
    #[default]
    Forwarding,
    /// The microphone owns the preedit. Keys keep going to IBus — they are
    /// rare mid-utterance and dropping them is worse than letting them
    /// through — but the preedit on screen is the recogniser's.
    Dictating,
}

/// One thing the frontend must do to the protocol.
///
/// Deliberately not a Wayland request: `Flush` is three conditions and a
/// method call, and `Commit` is a preedit clear and a commit inside one
/// double-buffered update. The point of the type is that a test can assert on
/// the *decision* without owning a compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Finish the conversion the keyboard left half-typed, if the engine
    /// asked us to hold it (`ClientCommitPreedit` with mode=commit). The
    /// user's 「こ」 becomes text in the field rather than disappearing.
    ///
    /// Only ever emitted while a field is active, which is not a detail:
    /// committing after a deactivation types the text into whichever window
    /// took focus next (phase-2 finding 10). The `Reset` path is the one case
    /// where the flush is both needed and legal, and it is this one.
    Flush,
    /// `IBusInputContext.Reset`: drop whatever composition the engine still
    /// holds, now that its text has been committed.
    Reset,
    /// Show this as the preedit, replacing whatever is there. The empty string
    /// clears it.
    Preedit(String),
    /// Commit this text, clearing the preedit in the same update.
    Commit(String),
}

/// Decides what one dictation command does.
///
/// The rules, in the order they are written:
///
/// 1. **`Begin` finishes the keyboard's turn before taking it.** A mozc
///    conversion in progress is committed (`Flush`) and only then reset, so
///    entering dictation mid-word costs the user nothing. With no field
///    focused there is nothing to finish and nothing to reset — and no preedit
///    to show either, so the router stays in `Forwarding` and the engine,
///    reading the same [`ImStatus`], types on the virtual keyboard instead.
/// 2. **A partial is only ever a preedit, and only while dictating.** One that
///    arrives in `Forwarding` is a command that raced its `Begin` past a focus
///    change; showing it would put voice text into a preedit the keyboard
///    believes it owns, so it is dropped. The commit that follows is not.
/// 3. **A commit is honoured in either state.** In `Dictating` it is the end
///    of the utterance. In `Forwarding` it is that same race one step later,
///    and the choice is between typing the user's sentence into the field they
///    are looking at or throwing it away; the sentence wins.
/// 4. **`Cancel` clears the preedit and nothing else.** No reset: the engine's
///    composition was already reset at `Begin`, and resetting again would only
///    discard whatever the user has typed since.
pub fn advance(state: Turn, command: &DictationCmd, active: bool) -> (Turn, Vec<Op>) {
    match (state, command, active) {
        (_, DictationCmd::Begin, true)  => (Turn::Dictating, vec![Op::Flush, Op::Reset]),
        (state, DictationCmd::Begin, false) => (state, Vec::new()),

        (Turn::Dictating, DictationCmd::Partial(text), true) => {
            (Turn::Dictating, vec![Op::Preedit(text.clone())])
        }
        (state, DictationCmd::Partial(_), _) => (state, Vec::new()),

        (_, DictationCmd::Commit(text), true) => {
            (Turn::Forwarding, vec![Op::Commit(text.clone())])
        }
        (_, DictationCmd::Commit(_), false) => (Turn::Forwarding, Vec::new()),

        (Turn::Dictating, DictationCmd::Cancel, _) => {
            (Turn::Forwarding, vec![Op::Preedit(String::new())])
        }
        (Turn::Forwarding, DictationCmd::Cancel, _) => (Turn::Forwarding, Vec::new()),
    }
}

// --- The link ---

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
    /// [`advance`] at the other end.
    pub fn is_active(&self) -> bool {
        self.bound.load(Ordering::Relaxed) && self.active.load(Ordering::Relaxed)
    }

    /// Records whether a frontend holds the slot. Called by the supervisor.
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

/// The engine's end of the connection to the input-method thread.
///
/// Cloneable and outlives any one frontend: the sender inside is *replaced*
/// each time the supervisor starts a frontend, because a calloop channel
/// belongs to the loop that polls it and a restarted loop is a new one. The
/// engine holds this from process start and never learns that a restart
/// happened — its sends go nowhere while `commands` is empty, which is exactly
/// what "the input method is not available" already means to it.
#[derive(Debug, Clone, Default)]
pub struct DictationLink {
    /// The running frontend's command channel, if one is running. The mutex is
    /// uncontended except at the instant of a restart; the alternative, a
    /// permanent channel plus a thread forwarding into a per-loop one, buys
    /// nothing but a thread.
    commands: Arc<Mutex<Option<Sender<DictationCmd>>>>,
    /// What the frontend says about itself.
    status  : Arc<ImStatus>,
}

impl DictationLink {
    /// A link with no frontend behind it yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The status flags, for the frontend and the supervisor to write.
    pub fn status(&self) -> &Arc<ImStatus> {
        &self.status
    }

    /// Whether dictation should use the input-method path right now.
    pub fn is_active(&self) -> bool {
        self.status.is_active()
    }

    /// Points the link at a freshly started frontend's channel.
    pub fn attach(&self, sender: Sender<DictationCmd>) {
        *self.commands.lock().expect("the dictation link mutex is never poisoned") = Some(sender);
    }

    /// Forgets the frontend's channel, after it stopped.
    pub fn detach(&self) {
        *self.commands.lock().expect("the dictation link mutex is never poisoned") = None;
    }

    /// Sends one command, dropping it if no frontend is running.
    ///
    /// Never blocks and never fails upwards: a calloop channel is unbounded,
    /// and a send that fails means the loop it belonged to is gone, which the
    /// engine already handles by falling back to the virtual keyboard.
    pub fn send(&self, command: DictationCmd) {
        let mut slot = self.commands.lock().expect("the dictation link mutex is never poisoned");
        let Some(sender) = slot.as_ref() else {
            tracing::debug!("dropping {command:?}: no input-method frontend is running");
            return;
        };
        if sender.send(command).is_err() {
            tracing::debug!("the input-method frontend stopped reading dictation commands");
            *slot = None;
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Entering dictation with a field focused finishes the keyboard's turn
    /// first. The order matters: the flush has to happen before the reset, or
    /// the composition it is trying to save is already gone.
    #[test]
    fn begin_finishes_the_conversion_then_resets() {
        let (state, ops) = advance(Turn::Forwarding, &DictationCmd::Begin, true);
        assert_eq!(state, Turn::Dictating);
        assert_eq!(ops, vec![Op::Flush, Op::Reset]);
    }

    /// With nothing focused there is no preedit to save, no context to reset
    /// and nowhere to show partials. The engine reads the same flag and types
    /// on the virtual keyboard instead, so staying in `Forwarding` is what
    /// keeps the two halves agreeing.
    #[test]
    fn begin_does_nothing_without_a_field() {
        let (state, ops) = advance(Turn::Forwarding, &DictationCmd::Begin, false);
        assert_eq!(state, Turn::Forwarding);
        assert!(ops.is_empty());
    }

    /// The streaming recogniser revises constantly, and every revision is just
    /// the next preedit.
    #[test]
    fn partials_become_the_preedit() {
        let (state, ops) = advance(
            Turn::Dictating,
            &DictationCmd::Partial("hello wor".into()),
            true,
        );
        assert_eq!(state, Turn::Dictating);
        assert_eq!(ops, vec![Op::Preedit("hello wor".into())]);
    }

    /// A partial that arrives while the keyboard has the turn is a command
    /// that raced a focus change. Showing it would overwrite a preedit mozc
    /// believes it owns.
    #[test]
    fn a_partial_outside_dictation_is_dropped() {
        let (state, ops) = advance(
            Turn::Forwarding,
            &DictationCmd::Partial("hello".into()),
            true,
        );
        assert_eq!(state, Turn::Forwarding);
        assert!(ops.is_empty());
    }

    /// A partial with the field gone is dropped rather than committed: there
    /// is nothing on screen to correct, and the engine is already typing this
    /// utterance somewhere else.
    #[test]
    fn a_partial_without_a_field_is_dropped() {
        let (state, ops) = advance(
            Turn::Dictating,
            &DictationCmd::Partial("hello".into()),
            false,
        );
        assert_eq!(state, Turn::Dictating);
        assert!(ops.is_empty());
    }

    /// The end of an utterance: the preedit goes, the text lands, the keyboard
    /// gets the turn back.
    #[test]
    fn a_commit_ends_the_turn() {
        let (state, ops) = advance(
            Turn::Dictating,
            &DictationCmd::Commit("hello world ".into()),
            true,
        );
        assert_eq!(state, Turn::Forwarding);
        assert_eq!(ops, vec![Op::Commit("hello world ".into())]);
    }

    /// The race, one step later than the dropped partial: the `Begin` found no
    /// field, so nothing entered `Dictating`, but by commit time there is one.
    /// Throwing the user's sentence away to keep the state machine tidy is the
    /// wrong trade.
    #[test]
    fn a_commit_is_honoured_even_outside_dictation() {
        let (state, ops) = advance(
            Turn::Forwarding,
            &DictationCmd::Commit("hello".into()),
            true,
        );
        assert_eq!(state, Turn::Forwarding);
        assert_eq!(ops, vec![Op::Commit("hello".into())]);
    }

    /// The field went away mid-utterance. The frontend cannot commit into it —
    /// smithay would give the text to whatever has focus now — so it gives the
    /// turn back and lets the engine's virtual-keyboard fallback have it.
    #[test]
    fn a_commit_without_a_field_falls_through() {
        let (state, ops) = advance(
            Turn::Dictating,
            &DictationCmd::Commit("hello".into()),
            false,
        );
        assert_eq!(state, Turn::Forwarding);
        assert!(ops.is_empty());
    }

    /// Cancelling clears the provisional text and stops there. Resetting the
    /// engine as well would throw away anything the user typed after the
    /// utterance began.
    #[test]
    fn cancel_only_clears_the_preedit() {
        let (state, ops) = advance(Turn::Dictating, &DictationCmd::Cancel, true);
        assert_eq!(state, Turn::Forwarding);
        assert_eq!(ops, vec![Op::Preedit(String::new())]);
    }

    /// A cancel with no utterance in flight — the engine cancels
    /// unconditionally on disable — must not clear a preedit that belongs to
    /// the keyboard.
    #[test]
    fn cancel_outside_dictation_does_nothing() {
        let (state, ops) = advance(Turn::Forwarding, &DictationCmd::Cancel, true);
        assert_eq!(state, Turn::Forwarding);
        assert!(ops.is_empty());
    }

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
}
