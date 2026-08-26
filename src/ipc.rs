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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
    pub state   : Option<Event>,
    /// Whether transcript logging is on.
    pub logging : bool,
    /// The trigger key in effect.
    pub trigger : u16,
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
                                let (state, logging, trigger) = {
                                    let snapshot = latest.lock().unwrap();
                                    (snapshot.state.clone(), snapshot.logging, snapshot.trigger)
                                };
                                let logging = Event::Logging { enabled: logging };
                                let trigger = Event::Trigger { code: trigger };
                                if write_event(&mut write, &logging).await.is_err()
                                    || write_event(&mut write, &trigger).await.is_err()
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
