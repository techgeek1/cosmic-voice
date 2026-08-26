//! A hand-written D-Bus client for ibus-daemon.
//!
//! This is the upstream leg of the input-method multiplexer designed in
//! `docs/multiplexer.md`: the half that talks to IBus, so that the half that
//! talks to Wayland (phase 2) can hand keys to mozc and get preedit, commits
//! and candidates back. Nothing here binds a Wayland protocol, renders
//! anything, or decides routing policy — those are later phases.
//!
//! Three things make this module more than a `zbus` boilerplate exercise:
//!
//! - **It is not the session bus.** ibus-daemon runs a *second, private*
//!   message bus of its own, whose socket is advertised in a file under
//!   `~/.config/ibus/bus/`. It is a full bus, not a peer-to-peer connection:
//!   SASL EXTERNAL, a real `Hello`, well-known names, unicast signals. See
//!   [`address`] for how the socket is found and [`bus`] for the connection.
//! - **Every interesting payload is an `IBusSerializable` object**, not a plain
//!   D-Bus type: a struct whose first two fields are a type name and an
//!   attachment dictionary, with the real fields appended positionally. Preedit
//!   text, candidate tables and engine descriptions all arrive this way, and
//!   [`text`] is the codec for them.
//! - **The key path is synchronous.** Since 1.5.28 the recommended client
//!   contract is: call `ProcessKeyEvent` and block; while the daemon is inside
//!   that call it *withholds* the effects the engine produced and queues them;
//!   read them back afterwards from a property. That removes the classic race
//!   where a commit signal overtakes the "not handled, pass it through"
//!   answer. [`context::Context::process_key`] implements it.
//!
//! # Threading
//!
//! Everything here is blocking, and `zbus`'s blocking API drives its own
//! runtime internally. That means **no method on any of these types may be
//! called from inside a tokio runtime** — `Runtime::block_on` panics when it is
//! re-entered. This is not a limitation in practice: the design puts the whole
//! IM stack on a dedicated calloop thread precisely because the key path
//! blocks, and the engine's tokio loop must never wait behind it.

mod address;
mod bus;
mod context;
mod text;

pub use address::Address;
pub use bus::Bus;
pub use context::{CAPABILITIES, Context, RELEASE_MASK, describe_capabilities};
pub use text::Text;

use std::path::PathBuf;
use std::time::Duration;

/// The client name new input contexts are introduced with.
///
/// The value is not cosmetic. ibus-daemon compares the first seven characters
/// of the client name against `"wayland"`
/// (`bus/inputcontext.c:389-398`, `IGNORE_FOCUS_OUT_CONDITION`) and, for
/// clients that do *not* match, latches an `ignore_focus_out` flag whenever a
/// preedit becomes visible in what it considers a Wayland session. With that
/// flag set, `FocusOut` and `Reset` become silent no-ops
/// (`bus/inputcontext.c:1307-1311` and `1332-1338`) — which would break the
/// multiplexer's turn-taking, where entering dictation resets the context.
///
/// The workaround exists for XIM and GTK clients that lose focus to IBus's own
/// candidate window; we are the compositor-side input method and never do. So
/// we opt out by keeping the prefix, while staying identifiable in traces.
///
/// (The condition also requires a "Wayland session", which the daemon defines
/// as "some panel has registered global shortcut keys",
/// `bus/ibusimpl.c:2667-2670`. That becomes true as soon as phase 4 registers
/// the engine-switch trigger, so the prefix must be right before then.)
pub const CLIENT_NAME: &str = "wayland-cosmic-voice";

// --- Errors ---

/// Everything that can go wrong between here and ibus-daemon.
///
/// Typed rather than `anyhow` because the callers above this module have to
/// *act* on the difference: a missing address file means IBus is not running
/// and the multiplexer should pass keys through, a dead PID means a stale file
/// to be ignored, and a D-Bus transport error means the daemon died and every
/// context must be rebuilt.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `IBUS_ADDRESS` was unset and `WAYLAND_DISPLAY` is not set either, so the
    /// address file's name cannot be constructed.
    #[error("neither IBUS_ADDRESS nor WAYLAND_DISPLAY is set; cannot locate ibus")]
    NoDisplay,

    /// The machine ID could not be read from either of its two standard homes.
    #[error("reading the machine id from /etc/machine-id or /var/lib/dbus/machine-id")]
    MachineId(#[source] std::io::Error),

    /// The address file does not exist. Normally means ibus-daemon is not
    /// running for this display.
    #[error("no ibus address file at {}", .0.display())]
    NoAddressFile(PathBuf),

    /// The address file exists but could not be read.
    #[error("reading {}", .path.display())]
    AddressFile {
        /// The file we tried to read.
        path  : PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// The address file was read but did not contain what it must.
    #[error("{}: {reason}", .path.display())]
    MalformedAddress {
        /// The file we parsed.
        path  : PathBuf,
        /// What was wrong with it.
        reason: String,
    },

    /// The address file names a daemon PID that no longer exists: a leftover
    /// from a previous session, pointing at a socket nobody is listening on.
    #[error("ibus-daemon pid {pid} from {} is not running", .path.display())]
    DaemonGone {
        /// The stale file.
        path: PathBuf,
        /// The PID it claimed.
        pid : u32,
    },

    /// The D-Bus transport or a peer returned an error.
    #[error("ibus d-bus call failed: {0}")]
    Dbus(#[from] zbus::Error),

    /// A standard `org.freedesktop.DBus` error, which is what property
    /// get/set failures surface as.
    #[error("ibus d-bus property access failed: {0}")]
    Fdo(#[from] zbus::fdo::Error),

    /// A serialised IBus object was not shaped the way its type says it is.
    #[error("decoding {what}: {reason}")]
    Decode {
        /// The IBus type name we were decoding.
        what  : &'static str,
        /// How it disagreed with the expected layout.
        reason: String,
    },
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, Error>;

// --- Signal plumbing ---

/// A blocking receiver for the raw D-Bus messages arriving on the ibus
/// connection.
///
/// IBus delivers everything interesting after the key path as signals, and
/// they are *unicast* — the daemon addresses them to our unique name, so no
/// match rules are involved and every one of them lands on this stream. We
/// decode from raw messages rather than from `zbus`'s generated signal streams
/// because one stream then covers all twenty-odd input-context signals with a
/// single `match`, and because the phase-2 calloop thread wants exactly one
/// pollable source per connection, not twenty.
///
/// **The stream must be drained.** `zbus` buffers a bounded queue of messages
/// per stream (64 by default) and stalls the connection when it fills, so a
/// [`Context`] whose signals nobody reads will eventually wedge the whole
/// client. Phase 2 replaces the timeout loop below with a calloop source that
/// drains on readiness; until then, callers poll.
struct Signals {
    /// The message stream, cloned off the live connection.
    stream: zbus::MessageStream,
}

impl Signals {
    /// Starts buffering messages from `connection`.
    ///
    /// Messages that arrived before this call are not replayed, so this has to
    /// happen before the first call that can provoke a signal.
    fn new(connection: &zbus::blocking::Connection) -> Self {
        Self {
            stream: zbus::MessageStream::from(connection.inner().clone()),
        }
    }

    /// Waits for the next message, up to `timeout`.
    ///
    /// `None` means the timeout expired or the connection closed; the two are
    /// deliberately not distinguished here because callers treat both as "stop
    /// draining for now" and notice a dead connection on their next method
    /// call, which is where a useful error comes from.
    fn next(&mut self, timeout: Option<Duration>) -> Result<Option<zbus::message::Message>> {
        use futures::StreamExt;

        // `zbus::block_on` rather than a runtime of our own. Polling a
        // `MessageStream` ticks the connection's internal executor, which owns
        // the tokio socket, so it has to happen inside the runtime zbus built
        // the connection on — doing it from a second runtime panics with
        // "there is no reactor running". The function is what zbus's own
        // documentation examples use for exactly this, and the runtime it
        // enters has both the IO and timer drivers enabled, which is what makes
        // `tokio::time::timeout` legal here.
        let next = self.stream.next();
        let message = match timeout {
            // The timeout future is built *inside* the block, not passed in:
            // constructing a tokio timer registers it with the current
            // runtime, and there is none until `block_on` enters one.
            Some(limit) => match zbus::block_on(async { tokio::time::timeout(limit, next).await }) {
                Ok(message) => message,
                Err(_)      => return Ok(None),
            },
            None => zbus::block_on(next),
        };

        match message {
            Some(Ok(message)) => Ok(Some(message)),
            Some(Err(e))      => Err(Error::Dbus(e)),
            None              => Ok(None),
        }
    }
}
