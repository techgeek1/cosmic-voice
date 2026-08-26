//! The dictation state machine, and the only place the pieces meet.
//!
//! Everything the applet knows about the engine arrives as `ipc::Event`, and
//! everything it asks for goes out as `ipc::Command`. Nothing here touches
//! libcosmic.
//!
//! The engine runs on its own thread with a single-threaded runtime. That is
//! not an aesthetic choice: the injector's Wayland event queue lives here, the
//! udev-backed key watcher and the recognisers each own threads of their own,
//! and keeping the coordination single-threaded means the state machine never
//! needs a lock. The applet talks to it exclusively through channels.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

use crate::asr::{AsrCmd, AsrEvent};
use crate::audio::Capture;
use crate::config::{Config, FallbackPartials};
use crate::hotkey::{HotkeyEvent, KeyEdge, Watcher};
use crate::inject::{Injector, common_prefix_len};
use crate::ipc::{Command, Event};
use crate::transcript_log;
use crate::vad::SilenceGate;

/// How often captured audio is drained to the recogniser while recording.
const TICK_MS: u64 = 60;

/// Where the engine is in the capture cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing in flight.
    Idle,
    /// Capturing. `held` records whether this utterance ends on key release
    /// (hold-to-talk) or on silence/toggle (press-once).
    Recording { held: bool },
    /// Offline model running on the final buffer. Further triggers are
    /// ignored rather than queued.
    Transcribing,
}

/// The applet's connection to a running engine.
#[derive(Clone)]
pub struct Handle {
    /// Requests into the state machine.
    pub commands : mpsc::Sender<Command>,
    /// State transitions out. Subscribe per consumer.
    pub events   : broadcast::Sender<Event>,
}

/// Starts the engine on its own thread and returns the applet's handle.
///
/// Startup errors surface as an [`Event::Failed`] on the handle rather than
/// here: the panel should come up and show the failure, not refuse to start.
///
/// Only one process per session actually runs the pipeline. The panel spawns
/// one applet instance per output, so each spawn first contends for the
/// primary lock: the winner owns hotkey, capture, and injection; the losers
/// run as mirrors of it over the control socket, so every panel icon works.
/// A mirror whose primary dies re-contends, which is also how the resident
/// models survive a panel restart on one output.
pub fn spawn(config: Config) -> Handle {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(16);
    let (evt_tx, _) = broadcast::channel::<Event>(64);
    let handle = Handle { commands: cmd_tx, events: evt_tx.clone() };

    std::thread::Builder::new()
        .name("voice-engine".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("building the engine runtime");

            runtime.block_on(async move {
                loop {
                    match crate::ipc::try_primary_lock() {
                        Ok(Some(_lock)) => {
                            match Engine::create(config.clone(), evt_tx.clone()) {
                                Ok(mut engine) => {
                                    if let Err(e) = engine.run(&mut cmd_rx).await {
                                        tracing::error!("engine stopped: {e:#}");
                                        let _ = evt_tx
                                            .send(Event::Failed { reason: format!("{e:#}") });
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("engine failed to start: {e:#}");
                                    let _ =
                                        evt_tx.send(Event::Failed { reason: format!("{e:#}") });
                                }
                            }
                            return;
                        }
                        Ok(None) => {
                            tracing::info!("another instance is primary; mirroring it");
                            match run_mirror(&mut cmd_rx, &evt_tx).await {
                                // Primary gone: contend for its role.
                                Ok(true)  => {}
                                // Our own handle is gone: the process is done.
                                Ok(false) => return,
                                Err(e)    => tracing::warn!("mirror failed: {e:#}"),
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        Err(e) => {
                            tracing::error!("primary arbitration failed: {e:#}");
                            let _ = evt_tx.send(Event::Failed { reason: format!("{e:#}") });
                            return;
                        }
                    }
                }
            });
        })
        .expect("spawning the engine thread");

    handle
}

/// Mirrors the primary over the control socket: its events feed our
/// subscribers, our commands feed it.
///
/// Returns `Ok(true)` when the primary went away (retry the lock), `Ok(false)`
/// when our own command channel closed (shut down).
async fn run_mirror(
    commands: &mut mpsc::Receiver<Command>,
    events: &broadcast::Sender<Event>,
) -> Result<bool> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let path = crate::ipc::socket_path()?;
    let stream = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to the primary at {}", path.display()))?;
    let (read, mut write) = stream.into_split();

    let mut frame = serde_json::to_vec(&Command::Subscribe).expect("commands always serialise");
    frame.push(b'\n');
    write.write_all(&frame).await.context("subscribing to the primary")?;

    let mut lines = BufReader::new(read).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => match serde_json::from_str::<Event>(&line) {
                    Ok(event) => { let _ = events.send(event); }
                    Err(e)    => tracing::warn!("malformed event frame: {e}"),
                },
                _ => return Ok(true),
            },
            cmd = commands.recv() => match cmd {
                Some(cmd) => {
                    let mut frame =
                        serde_json::to_vec(&cmd).expect("commands always serialise");
                    frame.push(b'\n');
                    if write.write_all(&frame).await.is_err() {
                        return Ok(true);
                    }
                }
                None => return Ok(false),
            },
        }
    }
}

// --- Engine ---

/// Owns every subsystem and the state machine that sequences them.
struct Engine {
    /// User settings, read at startup.
    config     : Config,
    /// Current position in the capture cycle.
    state      : State,
    /// Whether the trigger is armed. Disarmed, the engine ignores key edges
    /// and start commands; everything else keeps running so re-arming is
    /// instant.
    enabled    : bool,
    /// Whether committed transcripts are appended to the corpus log.
    logging    : bool,
    /// Whether a rebind is waiting for a key press. Key edges do not arrive
    /// while this is set; commands that would start capture are ignored.
    rebinding  : bool,
    /// Control over the hotkey watcher thread, once it is running.
    hotkey     : Option<crate::hotkey::Control>,
    /// Continuously running microphone capture.
    capture    : Capture,
    /// Commands into the recogniser thread.
    asr        : std::sync::mpsc::Sender<AsrCmd>,
    /// Results out of the recogniser thread.
    asr_events : mpsc::UnboundedReceiver<AsrEvent>,
    /// Wayland text injection.
    inject     : Injector,
    /// Trailing-silence detector for press-once mode.
    vad        : SilenceGate,
    /// State transitions published to the applet.
    events     : broadcast::Sender<Event>,
    /// Most recent state event, replayed to mirrors when they subscribe.
    latest     : crate::ipc::Latest,

    /// Ring position where the current utterance starts (pre-roll included).
    utt_start  : u64,
    /// Ring position up to which audio has been fed to the streaming model.
    cursor     : u64,
    /// Wall-clock start of the current utterance.
    started    : Instant,
    /// Seconds value last published in a `Recording` event, to throttle them.
    last_sec   : u64,
    /// Recent streaming hypotheses, for stability gating in `StreamOnly`.
    hyps       : VecDeque<String>,
    /// Text already typed via the virtual keyboard this utterance.
    vk_typed   : String,
    /// The streaming model's latest hypothesis this utterance, for the log.
    last_hyp   : String,
    /// Samples handed to the offline model, for the log.
    utt_len    : usize,
}

impl Engine {
    /// Builds an engine: starts capture, the recogniser thread, and the
    /// injector. Model loading continues in the background; the engine is
    /// usable once [`AsrEvent::Ready`] arrives.
    fn create(config: Config, events: broadcast::Sender<Event>) -> Result<Self> {
        let capture = Capture::start(config.preroll_ms).context("starting audio capture")?;
        let inject = Injector::connect(config.bind_input_method)
            .context("connecting the injector")?;
        tracing::info!("injector ready, input method: {}", inject.im_status());
        let (asr, asr_events) = crate::asr::spawn(&config);

        let snapshot = crate::ipc::Snapshot {
            state   : None,
            logging : config.log_transcripts,
            trigger : config.trigger_code,
        };

        Ok(Self {
            vad        : SilenceGate::new(config.silence_ms),
            state      : State::Idle,
            enabled    : true,
            logging    : config.log_transcripts,
            rebinding  : false,
            hotkey     : None,
            utt_start  : 0,
            cursor     : 0,
            started    : Instant::now(),
            last_sec   : 0,
            hyps       : VecDeque::new(),
            vk_typed   : String::new(),
            last_hyp   : String::new(),
            utt_len    : 0,
            config     : config,
            capture    : capture,
            asr        : asr,
            asr_events : asr_events,
            inject     : inject,
            events     : events,
            latest     : std::sync::Arc::new(std::sync::Mutex::new(snapshot)),
        })
    }

    /// Runs the state machine until every command source is gone.
    async fn run(&mut self, commands: &mut mpsc::Receiver<Command>) -> Result<()> {
        // The physical trigger, on its own thread because udev's socket is not
        // Send. A dead watcher leaves the socket-command path alive.
        let (key_tx, mut keys) = mpsc::channel::<HotkeyEvent>(16);
        let (mut watcher, control) =
            Watcher::new(self.config.trigger_code).context("creating the hotkey watcher")?;
        self.hotkey = Some(control);
        std::thread::Builder::new()
            .name("hotkey".into())
            .spawn(move || {
                if let Err(e) = watcher.run_blocking(key_tx) {
                    tracing::error!("hotkey watcher stopped: {e:#}");
                }
            })
            .context("spawning the hotkey watcher")?;

        // The control socket, for scripting, extra bindings, and mirrors.
        let (sock_tx, mut sock_rx) = mpsc::channel::<Command>(16);
        let sock_events = self.events.clone();
        let sock_latest = self.latest.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::ipc::serve(sock_tx, sock_events, sock_latest).await {
                tracing::error!("control socket: {e:#}");
            }
        });

        let mut tick = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                evt = keys.recv() => match evt {
                    Some(HotkeyEvent::Edge(edge))   => self.on_key(edge),
                    Some(HotkeyEvent::Rebound(code)) => self.on_rebound(code),
                    None => tracing::warn!("hotkey channel closed"),
                },
                cmd = commands.recv() => match cmd {
                    Some(cmd) => self.on_command(cmd),
                    None      => return Ok(()),
                },
                cmd = sock_rx.recv() => {
                    if let Some(cmd) = cmd {
                        self.on_command(cmd);
                    }
                },
                evt = self.asr_events.recv() => match evt {
                    Some(evt) => self.on_asr(evt),
                    None      => anyhow::bail!("recogniser thread died"),
                },
                _ = tick.tick(), if matches!(self.state, State::Recording { .. }) => {
                    self.on_tick();
                },
            }
        }
    }
}

impl Engine {
    /// Handles a physical trigger edge.
    fn on_key(&mut self, edge: KeyEdge) {
        if !self.enabled {
            return;
        }
        match (edge, self.state, self.config.hold_to_talk) {
            (KeyEdge::Pressed, State::Idle, true)        => self.start_recording(true),
            (KeyEdge::Released, State::Recording { held: true }, _) => self.finish_recording(),
            (KeyEdge::Pressed, State::Idle, false)       => self.start_recording(false),
            (KeyEdge::Pressed, State::Recording { held: false }, false) => self.finish_recording(),
            _ => {}
        }
    }

    /// Handles a command from the applet or the control socket.
    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Enable        => return self.set_enabled(true),
            Command::Disable       => return self.set_enabled(false),
            Command::SetLogging(on) => return self.set_logging(on),
            Command::Rebind        => return self.start_rebind(),
            _ if !self.enabled || self.rebinding => return,
            _                      => {}
        }
        match (cmd, self.state) {
            (Command::Start, State::Idle)                  => self.start_recording(false),
            (Command::Stop, State::Recording { .. })       => self.finish_recording(),
            (Command::Toggle, State::Idle)                 => self.start_recording(false),
            (Command::Toggle, State::Recording { .. })     => self.finish_recording(),
            (Command::Cancel, State::Recording { .. })
            | (Command::Cancel, State::Transcribing)       => self.cancel(),
            _ => {}
        }
    }

    /// Arms or disarms the trigger.
    ///
    /// Disarming aborts anything in flight rather than letting it finish: a
    /// transcription landing in some window after the user hit the kill
    /// switch is exactly what the kill switch exists to prevent.
    fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;

        if enabled {
            self.emit(Event::Idle);
            return;
        }
        // Late `Final`s are dropped because the state is no longer
        // `Transcribing`; see `on_asr`.
        let _ = self.inject.preedit("");
        self.state = State::Idle;
        self.emit(Event::Disabled);
    }

    /// Puts the watcher into capture mode for the next key press.
    ///
    /// Anything in flight is cancelled first: the key the user is about to
    /// press is a choice, not dictation, and the watcher stops reporting edges
    /// while it captures, so a held utterance could never be released anyway.
    fn start_rebind(&mut self) {
        let Some(control) = self.hotkey.clone() else {
            tracing::warn!("rebind requested before the hotkey watcher started");
            return;
        };
        if self.rebinding {
            return;
        }
        if self.state != State::Idle {
            self.cancel();
        }
        self.rebinding = true;
        control.capture();
        self.emit(Event::Rebinding);
    }

    /// Handles the watcher's answer to a rebind.
    fn on_rebound(&mut self, code: Option<u16>) {
        self.rebinding = false;
        if let Some(code) = code {
            self.config.trigger_code = code;
            if let Err(e) = Config::persist_trigger(code) {
                tracing::warn!("could not save the trigger: {e:#}");
                self.emit(Event::Failed { reason: format!("trigger not saved: {e:#}") });
            }
        }
        self.emit(Event::Trigger { code: self.config.trigger_code });
        self.emit(if self.enabled { Event::Idle } else { Event::Disabled });
    }

    /// Switches transcript logging.
    fn set_logging(&mut self, enabled: bool) {
        if self.logging == enabled {
            return;
        }
        self.logging = enabled;
        tracing::info!("transcript logging {}", if enabled { "on" } else { "off" });
        self.emit(Event::Logging { enabled: enabled });
    }

    /// Begins capture at `now - preroll`.
    fn start_recording(&mut self, held: bool) {
        self.utt_start = self.capture.mark_preroll();
        self.cursor = self.utt_start;
        self.started = Instant::now();
        self.last_sec = 0;
        self.vad.reset();
        self.hyps.clear();
        self.vk_typed.clear();
        self.last_hyp.clear();
        let _ = self.asr.send(AsrCmd::Reset);

        self.state = State::Recording { held: held };
        self.emit(Event::Recording { elapsed_ms: 0 });
    }

    /// Drains new audio to the streaming model and enforces the end rules.
    fn on_tick(&mut self) {
        let State::Recording { held } = self.state else { return };

        let now = self.capture.now();
        if now > self.cursor {
            let chunk = self.capture.read(self.cursor, now);
            self.cursor = now;

            // Silence only ends the utterance when no key is holding it open;
            // in hold mode the human's finger is the endpointer.
            if self.vad.push(&chunk) && !held {
                self.finish_recording();
                return;
            }
            let _ = self.asr.send(AsrCmd::Feed(chunk));
        }

        let elapsed = self.started.elapsed();
        if elapsed.as_secs() >= self.config.max_utterance_s as u64 {
            tracing::warn!("utterance hit the length cap");
            self.finish_recording();
            return;
        }
        if elapsed.as_secs() != self.last_sec {
            self.last_sec = elapsed.as_secs();
            self.emit(Event::Recording { elapsed_ms: elapsed.as_millis() as u64 });
        }
    }

    /// Ends capture and hands the whole utterance to the offline model.
    fn finish_recording(&mut self) {
        let samples = self.capture.read(self.utt_start, self.capture.now());
        let hotwords = self.config.default_hotwords.clone();
        self.utt_len = samples.len();
        let _ = self.asr.send(AsrCmd::Finalize { samples: samples, hotwords: hotwords });

        self.state = State::Transcribing;
        self.emit(Event::Transcribing);
    }

    /// Discards the in-flight utterance.
    fn cancel(&mut self) {
        // Clear any preedit the partials put up. Best effort; the compositor
        // clears it anyway when focus moves.
        let _ = self.inject.preedit("");

        self.state = State::Idle;
        self.emit(Event::Idle);
    }

    /// Handles recogniser output.
    fn on_asr(&mut self, evt: AsrEvent) {
        match evt {
            AsrEvent::Ready => {
                tracing::info!("models loaded");
                self.emit(Event::Logging { enabled: self.logging });
                self.emit(Event::Trigger { code: self.config.trigger_code });
                self.emit(if self.enabled { Event::Idle } else { Event::Disabled });
            }
            AsrEvent::LoadFailed(reason) => {
                self.emit(Event::Failed { reason: reason });
            }
            AsrEvent::Partial(text) => {
                // A partial that raced the finalize is stale; drop it.
                if matches!(self.state, State::Recording { .. }) {
                    self.emit(Event::Partial { text: text.clone() });
                    self.show_partial(&text);
                    self.last_hyp = text;
                }
            }
            AsrEvent::Final(Ok(text)) => {
                if self.state == State::Transcribing {
                    self.commit(&text);
                }
            }
            AsrEvent::Final(Err(reason)) => {
                // Only report a failure someone is waiting on; a cancelled or
                // disarmed utterance's error is just noise.
                if self.state == State::Transcribing {
                    self.state = State::Idle;
                    self.emit(Event::Failed { reason: reason });
                    self.emit(Event::Idle);
                }
            }
        }
    }

    /// Routes provisional text to whichever display the focus allows.
    fn show_partial(&mut self, text: &str) {
        // Preedit first: free revision, replaced wholesale at commit.
        match self.inject.preedit(text) {
            Ok(true)  => return,
            Ok(false) => {}
            Err(e)    => {
                tracing::warn!("preedit failed: {e:#}");
                return;
            }
        }

        // No preedit. Either wait for the final, or type the stable prefix
        // and stand behind it.
        if self.config.fallback_partials != FallbackPartials::StreamOnly {
            return;
        }

        self.hyps.push_back(text.to_owned());
        let window = self.config.stability_frames as usize + 1;
        while self.hyps.len() > window {
            self.hyps.pop_front();
        }
        if self.hyps.len() < window {
            return;
        }

        // The stable prefix is what every recent hypothesis agrees on, cut
        // back to a word boundary so a half-formed word never lands.
        let mut stable_len = self.hyps[0].len();
        for pair in self.hyps.iter().zip(self.hyps.iter().skip(1)) {
            stable_len = stable_len.min(common_prefix_len(pair.0, pair.1));
        }
        let stable = &self.hyps[0][..stable_len];
        let stable = match stable.rfind(' ') {
            Some(idx) => &stable[..=idx],
            None      => return,
        };

        // Emit only what extends what was already typed. A hypothesis that
        // diverges from typed text is dropped: the text is out, and a wrong
        // correction is worse than a missing one.
        if let Some(tail) = stable.strip_prefix(self.vk_typed.as_str())
            && !tail.is_empty()
        {
            if let Err(e) = self.inject.type_text(tail) {
                tracing::warn!("typing stable prefix failed: {e:#}");
                return;
            }
            self.vk_typed.push_str(tail);
        }
    }

    /// Injects the final transcript and returns to idle.
    fn commit(&mut self, text: &str) {
        if text.is_empty() {
            // Silence in, nothing out. Clear any provisional text.
            let _ = self.inject.preedit("");
            self.state = State::Idle;
            self.emit(Event::Idle);
            return;
        }

        let mut out = text.to_owned();
        if self.config.trailing_space {
            out.push(' ');
        }

        let injected = match self.inject.commit_im(&out) {
            Ok(true)  => Ok(()),
            Ok(false) => {
                // Virtual-keyboard path: emit whatever the stable prefix has
                // not already typed. Divergence keeps the streamed text.
                match out.strip_prefix(self.vk_typed.as_str()) {
                    Some(tail) => self.inject.type_text(tail),
                    None       => {
                        tracing::info!(
                            "final diverges from typed prefix; keeping streamed text"
                        );
                        Ok(())
                    }
                }
            }
            Err(e) => Err(e),
        };

        match injected {
            Ok(())  => self.emit(Event::Injected { text: text.to_owned() }),
            Err(e)  => self.emit(Event::Failed { reason: format!("{e:#}") }),
        }
        self.state = State::Idle;
        self.emit(Event::Idle);

        // Logged regardless of whether injection succeeded: the transcript is
        // the data, and a focus-related injection failure says nothing about it.
        if self.logging {
            let record = transcript_log::Record {
                ts       : transcript_log::now(),
                audio_ms : self.utt_len as u64 * 1000 / crate::audio::SAMPLE_RATE as u64,
                partial  : &self.last_hyp,
                text     : text,
            };
            if let Err(e) = transcript_log::append(&crate::config::transcript_log_path(), &record) {
                tracing::warn!("transcript log: {e:#}");
            }
        }
    }

    /// Publishes an event, ignoring the no-subscriber case.
    fn emit(&self, event: Event) {
        // Partials churn too fast to be a useful snapshot and are meaningless
        // outside a recording; everything else is state a late mirror needs.
        match &event {
            Event::Partial { .. }       => {}
            Event::Logging { enabled }  => self.latest.lock().unwrap().logging = *enabled,
            Event::Trigger { code }     => self.latest.lock().unwrap().trigger = *code,
            _ => self.latest.lock().unwrap().state = Some(event.clone()),
        }
        let _ = self.events.send(event);
    }
}
