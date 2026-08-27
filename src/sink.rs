//! Where dictated text goes, and the one place that decides.
//!
//! There are two ways to put text into a focused window, and neither is right
//! for every window. `zwp_input_method_v2` gives free revision — a preedit can
//! be replaced as often as the recogniser changes its mind, and only the last
//! version is ever committed — but it reaches only clients that speak
//! text-input-v3, and the slot it needs is exclusive per seat. The virtual
//! keyboard reaches everything and revises nothing.
//!
//! Before the multiplexer, [`crate::inject::Injector`] held both and answered
//! "is the input-method path usable?" from its own Wayland connection. It
//! still holds the virtual keyboard, and it still answers that question when
//! `input_method: Off`. What changed is that the input-method path now lives
//! on another thread, behind [`crate::im`], because it is shared with IBus —
//! so the question has a second possible answer and somebody has to hold both.
//! That somebody is this file, and [`crate::engine`] asks it rather than
//! asking either path directly.
//!
//! # The rule that is structural, not conventional
//!
//! **In [`InputMethod::Multiplexer`] the injector is constructed with
//! `bind_im = false`.** Not "we take care not to call `preedit`" — the
//! injector never binds `zwp_input_method_v2` at all, so there is no object to
//! call it on and no way for a future edit here to reintroduce the second
//! binder. Two binders on one seat is the failure this whole subsystem exists
//! to avoid: under the smithay revision cosmic-comp pins, the second one
//! evicts the first, the first drops its input method but not its keyboard
//! grab, and every key on the session routes into a grab nobody services.
//! Making that unreachable by construction is worth more than a comment.

use anyhow::{Context, Result};

use crate::config::{Config, InputMethod};
use crate::im::{DictationCmd, DictationLink};
use crate::inject::Injector;

// --- The sink ---

/// The engine's outlet for text, over whichever path is live.
pub struct TextSink {
    /// The virtual keyboard, always present. It is the fallback for every
    /// client that cannot take a preedit, and the only path at all when
    /// `input_method: Off` leaves nobody holding the slot.
    inject: Injector,
    /// The multiplexer's frontend, when the config asked for one. Sends are
    /// dropped while no frontend is running, which is the same state the
    /// engine already handles by falling back.
    im    : Option<DictationLink>,
}

impl TextSink {
    /// Connects the injector and, in [`InputMethod::Multiplexer`], the
    /// input-method thread.
    ///
    /// Failing to reach the compositor is fatal, because then there is no way
    /// to type at all. Failing to bind the input method is not: the supervisor
    /// reports it and the virtual keyboard carries on.
    pub fn connect(config: &Config, link: Option<DictationLink>) -> Result<Self> {
        // `false`, unconditionally, in both modes. See the module docs.
        let inject = Injector::connect(false).context("connecting the injector")?;

        Ok(Self {
            inject: inject,
            im    : match config.input_method {
                InputMethod::Off         => None,
                InputMethod::Multiplexer => link,
            },
        })
    }

    /// Whether text should go through the input method at this instant.
    ///
    /// Read immediately before each use rather than cached, because the answer
    /// changes with focus and the whole point of asking is to catch the field
    /// going away mid-utterance.
    fn im_active(&self) -> bool {
        self.im.as_ref().is_some_and(DictationLink::is_active)
    }

    /// Announces the start of an utterance, so the input method can finish
    /// whatever the keyboard left half-typed and reset its engine.
    ///
    /// A no-op on the virtual-keyboard path: there is no state there to take
    /// a turn from.
    pub fn begin(&mut self) {
        if let Some(link) = self.im.as_ref() {
            link.send(DictationCmd::Begin);
        }
    }

    /// Shows provisional text, and reports whether anybody could.
    ///
    /// `false` means the preedit path is unavailable and the caller decides
    /// what the fallback policy is — which is the same contract
    /// [`Injector::preedit`] has always had, so `engine.rs` did not have to
    /// learn a new one.
    pub fn preedit(&mut self, text: &str) -> Result<bool> {
        if self.im_active() {
            self.im
                .as_ref()
                .expect("im_active implies a link")
                .send(DictationCmd::Partial(text.to_owned()));
            return Ok(true);
        }

        self.inject.preedit(text)
    }

    /// Commits final text through the input method, if there is one.
    ///
    /// `false` means it went nowhere and the caller should type it. The
    /// interesting case is the field disappearing between the start of the
    /// utterance and its end: the frontend's answer flips to inactive, this
    /// returns `false`, and the transcript lands on the virtual keyboard
    /// instead of into whichever window took focus.
    pub fn commit(&mut self, text: &str) -> Result<bool> {
        if self.im_active() {
            self.im
                .as_ref()
                .expect("im_active implies a link")
                .send(DictationCmd::Commit(text.to_owned()));
            return Ok(true);
        }

        self.inject.commit_im(text)
    }

    /// Abandons the utterance, clearing anything provisional it put up.
    ///
    /// Sent to both paths rather than to the live one: a cancel that races a
    /// focus change should clear the preedit wherever it ended up, and both
    /// calls are no-ops when there is nothing to clear.
    pub fn cancel(&mut self) {
        if let Some(link) = self.im.as_ref() {
            link.send(DictationCmd::Cancel);
        }
        if let Err(e) = self.inject.preedit("") {
            tracing::debug!("clearing the preedit: {e:#}");
        }
    }

    /// Types text verbatim through the virtual keyboard.
    ///
    /// Never routed through the input method: this is the path for text that
    /// is already final and already partly on screen, and the input method's
    /// value is revision.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        self.inject.type_text(text)
    }

    /// Human-readable state, for the startup log line.
    pub fn status(&self) -> String {
        match self.im.as_ref() {
            None       => format!("virtual keyboard only ({})", self.inject.im_status()),
            Some(link) => format!(
                "multiplexer, {}",
                if link.is_active() { "field focused" } else { "no field focused yet" },
            ),
        }
    }
}
