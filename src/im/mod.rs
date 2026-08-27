//! The Wayland input-method frontend: the multiplexer's downstream leg.
//!
//! [`crate::ibus`] is the upstream leg — it knows how to make an engine turn
//! `k`, `o` into こ. This is the half that owns the seat's
//! `zwp_input_method_v2` slot, takes the keyboard when a text field focuses,
//! and puts the engine's answers into the application. Between the two there
//! is one real decision, which lives in [`router`]: for a key the engine did
//! *not* consume, does the application see committed text or a replayed key?
//!
//! Named `im` rather than `frontend` because the codebase already has a
//! frontend in the everyday sense — `app.rs`, the panel applet — and because
//! `im` is what the protocol, the compositor and IBus all call this role. It
//! sits beside `ibus` as the other half of the same subsystem.
//!
//! # What is here
//!
//! - [`frontend`] — the connection, the activation lifecycle, the per-activation
//!   grab and virtual keyboard, key repeat, and the mapping from IBus's effects
//!   onto protocol requests. The event loop lives here.
//! - [`keyboard`] — the XKB keymap the grab sends us, and the three different
//!   keycode conventions that meet at it.
//! - [`router`] — the routing rules, as a pure function with tests. The only
//!   part of the leg that can be checked without a compositor.
//! - [`link`] — when to build an IBus context, and how its asynchronous signals
//!   reach the loop.
//! - [`switcher`] — the panel duties: parsing the engine-switch accelerators,
//!   registering them with the daemon, and cycling engines when one fires.
//! - [`content_type`] — text-input-v3's idea of what a field is, translated to
//!   IBus's.
//!
//! # Not here yet
//!
//! Candidate rendering (phase 3) and dictation turn-taking (phase 5). With
//! phase 2 alone, typing through mozc works but the candidate window is
//! invisible, so it is a milestone rather than something to switch to as a
//! daily driver.
//!
//! # Safety
//!
//! Binding this protocol on a seat that already has an input method is not a
//! failure that reports itself. Under the smithay revision cosmic-comp pins,
//! the *existing* holder is told it is unavailable, destroys its input method
//! but not its keyboard grab, and every key on the session then routes into a
//! grab nobody services — a dead keyboard until that process is killed. There
//! is no safe probe. So [`frontend::run`] takes the display name as an
//! argument, refuses the one the process inherited unless explicitly told
//! otherwise, and refuses outright if IBus's own Wayland bridge is running on
//! that display. See `docs/multiplexer.md` for the full incident.

mod content_type;
mod frontend;
mod keyboard;
mod link;
mod router;
mod switcher;

pub use frontend::{Options, run};
pub use switcher::ImEvent;
