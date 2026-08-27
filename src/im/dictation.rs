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
//! The plumbing that gets a [`DictationCmd`] from the engine's tokio loop
//! onto the frontend's calloop loop, and the one fact travelling the other
//! way, is [`super::command`]: the same road now carries the popup's engine
//! switch and menu activations, so it is no longer dictation's alone.

use serde::{Deserialize, Serialize};

// --- The commands ---

/// What the dictation engine asks the input method to do.
///
/// Serialisable because the harness feeds these in from a shell script through
/// a fifo (`devtest im-frontend --control-fifo`), which is what lets the
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
/// the engine notices the field is gone from [`super::command::ImStatus`] and
/// finishes on the virtual keyboard instead.
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
///    reading the same [`super::command::ImStatus`], types on the virtual
///    keyboard instead.
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
}
