//! The boundary between the applet shell and the dictation engine.
//!
//! Both halves currently live in one process, but every interaction between
//! them goes through these two enums. That keeps the eventual split into a
//! standalone daemon plus a thin applet a refactor rather than a rewrite: the
//! panel restarts applets on config change, which would otherwise drop the
//! resident ASR model and reintroduce multi-second load latency per utterance.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Requests that drive the engine's state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// Begin capturing. Idempotent while already capturing.
    Start,
    /// Stop capturing and transcribe what was captured.
    Stop,
    /// Start if idle, stop if capturing.
    Toggle,
    /// Stop capturing and discard the buffer without transcribing.
    Cancel,
    /// Arm the trigger. No-op while armed.
    Enable,
    /// Disarm the trigger and abort anything in flight. The escape hatch for
    /// screen shares and for applications that want the trigger key.
    Disable,
    /// Turn this connection into a mirror: the server starts streaming every
    /// `Event` back as newline JSON, and keeps reading commands. Used by the
    /// non-primary applet instances the panel spawns (one per output).
    Subscribe,
    /// Start or stop appending committed transcripts to the corpus log.
    /// Independent of `Enable`/`Disable`; accepted while disarmed.
    SetLogging(bool),
    /// Take the next key pressed on any keyboard as the new trigger. Aborts
    /// anything in flight; answered by `Event::Trigger` either way.
    Rebind,
    /// Switch IBus to the named engine, e.g. `mozc-jp`. Global, like the
    /// hotkey: every window moves at once. Confirmed by the next
    /// `InputMethod` event naming it, not by anything sent back here.
    SetEngine(String),
    /// Activate one entry of the engine's status menu, by key, with the state
    /// it should take (`ImPropState` as a number: 0 unchecked, 1 checked).
    /// The engine answers by updating the menu, which arrives as the next
    /// `InputMethod` event.
    ActivateProperty {
        /// The property's key, e.g. `InputMode.Hiragana`.
        key  : String,
        /// The state to set. Radios and mozc's input modes want 1.
        state: u32,
    },
}

/// Engine state transitions, consumed by the applet to drive the panel icon.
///
/// These are broadcast rather than request/response. The applet is one
/// subscriber among possibly several, and must never be able to block the
/// engine by failing to drain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Nothing in flight. The steady state.
    Idle,
    /// Capturing audio. Carries elapsed millis for the panel indicator.
    Recording { elapsed_ms: u64 },
    /// Audio captured, model running on the final buffer.
    Transcribing,
    /// Provisional text from a partial decode. May revise earlier words.
    Partial { text: String },
    /// Text was injected successfully. Carries the injected text for history.
    Injected { text: String },
    /// Something failed. The engine returns to `Idle` after emitting this.
    Failed { reason: String },
    /// The trigger is disarmed; key presses and start commands are ignored
    /// until `Enable`. Ends with an `Idle` when re-armed.
    Disabled,
    /// Transcript logging was switched, or is being reported to a new
    /// subscriber. Orthogonal to the capture state above.
    Logging { enabled: bool },
    /// Waiting for the user to press the key that becomes the new trigger.
    /// Ends with a `Trigger`.
    Rebinding,
    /// The trigger key in effect: at startup, after a rebind, or reported to
    /// a new subscriber. Orthogonal to the capture state.
    Trigger { code: u16 },
    /// Where the input-method multiplexer is. Orthogonal to the capture state
    /// as well: dictation works in every one of these, only the path changes.
    InputMethod { state: InputMethodState },
}

/// What the input-method multiplexer is doing, as the popup renders it.
///
/// This exists because the two things that can go wrong with owning a seat's
/// input-method slot are both invisible otherwise. IBus's bridge still holding
/// it is a log line nobody reads and a preedit that silently never appears;
/// the frontend dying is the same. Both are states the user has to be told
/// about, in a sentence, in the place they already look when dictation
/// behaves oddly.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InputMethodState {
    /// `input_method: Off`. Nothing binds the slot; dictation types.
    #[default]
    Off,
    /// Configured, but the slot belongs to somebody else. The reason names
    /// them and says what to do about it.
    Blocked { reason: String },
    /// Bound and running. Everything the popup's "Input method" section
    /// draws is in here, so a mirror gets it through the snapshot like the
    /// rest of the state.
    Running {
        /// The IBus engine in effect, once the daemon has said which.
        engine   : Option<ImEngine>,
        /// The engines the switch hotkey cycles through, in cycle order.
        engines  : Vec<ImEngine>,
        /// The mode glyph, e.g. `あ`: the `symbol` of the property whose key
        /// is the engine's `icon_prop_key`, updated as the engine changes
        /// mode. `None` for an engine with no such property.
        indicator: Option<String>,
        /// The engine's status menu, top level. Empty for an xkb engine.
        menu     : Vec<ImProperty>,
    },
    /// The frontend stopped and the supervisor is backing off before its next
    /// attempt. Dictation is on the virtual-keyboard path meanwhile.
    Stopped { reason: String },
}

/// The IBus engine in effect, for the status line.
///
/// Three strings because the daemon gives three and each is right in a
/// different place: `name` is what a config file spells (`mozc-jp`), `symbol`
/// is what a status area shows (`あ`), and `longname` is what a human calls it
/// (`Mozc`). The panel wants the last two and falls back to the first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImEngine {
    /// Engine id, e.g. `mozc-jp`.
    pub name    : String,
    /// Short status-area symbol, e.g. `あ`. Often empty for xkb engines.
    pub symbol  : String,
    /// Human-readable name, e.g. `Mozc`.
    pub longname: String,
}

/// How one status-menu entry is drawn (`IBusPropType`, `src/ibusproperty.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImPropKind {
    /// A plain action.
    Normal,
    /// On or off.
    Toggle,
    /// One of a group of siblings.
    Radio,
    /// A submenu; `children` are its entries.
    Menu,
    /// Spacing.
    Separator,
}

/// Whether a toggle or radio is on (`IBusPropState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImPropState {
    /// Off.
    Unchecked,
    /// On.
    Checked,
    /// Neither. Engines rarely send it.
    Inconsistent,
}

impl ImPropState {
    /// The state a toggle takes when switched on or off.
    pub fn from_bool(checked: bool) -> Self {
        if checked { ImPropState::Checked } else { ImPropState::Unchecked }
    }

    /// The number the engine wants back in `ActivateProperty`.
    pub fn as_u32(self) -> u32 {
        match self {
            ImPropState::Unchecked    => 0,
            ImPropState::Checked      => 1,
            ImPropState::Inconsistent => 2,
        }
    }
}

/// One entry of an IBus engine's status menu, as the popup draws it.
///
/// A serde mirror of `IBusProperty` with the fields the applet has a use for:
/// `icon` and `tooltip` are dropped, and the two enums replace the numeric
/// type and state so that the popup cannot draw an unknown value as a button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImProperty {
    /// What `ActivateProperty` takes, e.g. `InputMode.Hiragana`.
    pub key      : String,
    /// The text to show. Falls back to the key when the engine sent none.
    pub label    : String,
    /// How to draw it.
    pub kind     : ImPropKind,
    /// On or off, for toggles and radios.
    pub state    : ImPropState,
    /// Whether it can be activated.
    pub sensitive: bool,
    /// Whether to draw it at all.
    pub visible  : bool,
    /// Short status text, e.g. `あ`. Empty for most entries.
    pub symbol   : String,
    /// The entries of a `Menu`; empty for everything else.
    pub children : Vec<ImProperty>,
}

impl std::fmt::Display for ImEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = if self.longname.is_empty() { &self.name } else { &self.longname };
        write!(f, "{label}")?;
        if !self.symbol.is_empty() {
            write!(f, " {}", self.symbol)?;
        }

        Ok(())
    }
}

/// Returns the socket path, `$XDG_RUNTIME_DIR/cosmic-voice.sock`.
///
/// Runtime dir rather than a state dir because the socket must not outlive the
/// session, and logind cleans that directory for us on logout.
pub fn socket_path() -> Result<PathBuf> {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR unset; cannot locate the control socket")?;

    Ok(PathBuf::from(dir).join("cosmic-voice.sock"))
}

/// Tries to become the single engine primary.
///
/// The panel spawns one applet process per output, and every one of them runs
/// this code; without an arbiter each would watch the hotkey and inject,
/// multiplying every utterance by the monitor count. An `flock` on a runtime
/// file is the arbiter: it is atomic (unlike test-then-bind on the socket),
/// dies with the process, and costs nothing. Returns the held lock — keep it
/// alive for as long as the primary role is claimed — or `None` if another
/// live instance holds it.
pub fn try_primary_lock() -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;

    let dir = std::env::var("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR unset; cannot locate the engine lock")?;
    let path = PathBuf::from(dir).join("cosmic-voice.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    // SAFETY: plain syscall on an owned fd.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 { Ok(Some(file)) } else { Ok(None) }
}

/// Sends one command to a running instance and exits.
///
/// Used by the client role, so it is deliberately synchronous and has no
/// runtime. A missing socket is a plain error rather than an autostart: the
/// applet's lifetime belongs to the panel, not to a stray CLI invocation.
pub fn send_blocking(cmd: Command) -> Result<()> {
    use std::io::Write;

    let path = socket_path()?;
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("connecting to {}; is the applet running?", path.display()))?;

    let mut frame = serde_json::to_vec(&cmd).context("encoding the command")?;
    frame.push(b'\n');
    stream.write_all(&frame).context("sending the command")?;

    Ok(())
}

/// What a late mirror needs to become current.
///
/// Two independent pieces because the event stream carries two independent
/// things: where the capture cycle is, and whether logging is on. Replaying
/// only the most recent event would lose whichever one it was not.
#[derive(Debug, Default)]
pub struct Snapshot {
    /// Most recent capture-state event.
    pub state        : Option<Event>,
    /// Whether transcript logging is on.
    pub logging      : bool,
    /// The trigger key in effect.
    pub trigger      : u16,
    /// Where the input-method multiplexer is. Only the primary runs a
    /// frontend, so this is the only way a mirror can know.
    pub input_method : InputMethodState,
}

/// The engine's snapshot, shared with the control socket.
pub type Latest = std::sync::Arc<std::sync::Mutex<Snapshot>>;

/// Accepts connections for the process lifetime, forwarding parsed commands.
///
/// One newline-delimited JSON frame per line; a malformed line drops the
/// connection rather than the listener. A stale socket from a crashed
/// predecessor is removed first — safe because only the holder of the primary
/// lock ever runs this. A `Subscribe` frame upgrades the connection to full
/// duplex: events stream back while commands keep arriving.
pub async fn serve(
    tx: tokio::sync::mpsc::Sender<Command>,
    events: tokio::sync::broadcast::Sender<Event>,
    latest: Latest,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let path = socket_path()?;
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("binding {}", path.display()))?;

    loop {
        let (stream, _) = listener.accept().await.context("accepting on the control socket")?;
        let tx = tx.clone();
        let events = events.clone();
        let latest = latest.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let mut rx: Option<tokio::sync::broadcast::Receiver<Event>> = None;
            loop {
                tokio::select! {
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => match serde_json::from_str::<Command>(&line) {
                            Ok(Command::Subscribe) if rx.is_none() => {
                                rx = Some(events.subscribe());
                                // Catch the mirror up before live events flow.
                                let (state, logging, trigger, input_method) = {
                                    let snapshot = latest.lock().unwrap();
                                    (
                                        snapshot.state.clone(),
                                        snapshot.logging,
                                        snapshot.trigger,
                                        snapshot.input_method.clone(),
                                    )
                                };
                                let logging = Event::Logging { enabled: logging };
                                let trigger = Event::Trigger { code: trigger };
                                let input_method = Event::InputMethod { state: input_method };
                                if write_event(&mut write, &logging).await.is_err()
                                    || write_event(&mut write, &trigger).await.is_err()
                                    || write_event(&mut write, &input_method).await.is_err()
                                {
                                    return;
                                }
                                if let Some(event) = state
                                    && write_event(&mut write, &event).await.is_err()
                                {
                                    return;
                                }
                            }
                            Ok(cmd) => {
                                if tx.send(cmd).await.is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("malformed control frame: {e}");
                                return;
                            }
                        },
                        _ => return,
                    },
                    event = async { rx.as_mut().unwrap().recv().await }, if rx.is_some() => {
                        match event {
                            Ok(event) => {
                                if write_event(&mut write, &event).await.is_err() {
                                    return;
                                }
                            }
                            // Skipped a few under load; the next event carries
                            // fresh state.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        }
                    },
                }
            }
        });
    }
}

/// Writes one event frame.
async fn write_event(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    event: &Event,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut frame = serde_json::to_vec(event).expect("events always serialise");
    frame.push(b'\n');
    write.write_all(&frame).await
}
