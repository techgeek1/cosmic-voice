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

use crate::asr::{AsrEvent, OfflineCmd, StreamCmd};
use crate::audio::Capture;
use crate::config::{Config, FallbackPartials, InputMethod};
use crate::hotkey::{HotkeyEvent, KeyEdge, Watcher};
use crate::im::{ImCmd, ImEvent, ImLink};
use crate::inject::common_prefix_len;
use crate::ipc::{Command, Event, InputMethodState};
use crate::sink::TextSink;
use crate::transcript_log;
use crate::vad::{SegmentGate, SilenceGate};

/// How often captured audio is drained to the recogniser while recording.
pub const TICK_MS: u64 = 60;

/// Where the engine is in the capture cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing in flight.
    Idle,
    /// Capturing. `held` records whether this utterance ends on key release
    /// (hold-to-talk) or on silence/toggle (press-once).
    Recording { held: bool },
    /// Offline model running on the last segment. Further triggers are
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

/// Starts the input-method thread, in the one process that may run it.
///
/// Only the primary reaches here: [`Engine::create`] is called by the holder
/// of the flock, and every other applet instance is a mirror that never
/// constructs an engine at all. That is the enforcement — a seat has one
/// input-method slot and the panel spawns one applet per output, so "the
/// mirrors must not bind" is not a rule anybody has to remember.
///
/// Returns the process's end of the command link and the frontend's event
/// stream, or `(None, None)` when the config leaves the slot alone.
fn start_input_method(
    config: &Config,
) -> (Option<ImLink>, Option<mpsc::UnboundedReceiver<ImEvent>>) {
    if config.input_method != InputMethod::Multiplexer {
        return (None, None);
    }

    let Ok(display) = std::env::var("WAYLAND_DISPLAY") else {
        tracing::error!(
            "input_method: Multiplexer needs a Wayland session; WAYLAND_DISPLAY is unset"
        );
        return (None, None);
    };

    let link = ImLink::new();
    let (events, receiver) = mpsc::unbounded_channel::<ImEvent>();
    crate::im::spawn(crate::im::Supervised {
        display : display,
        triggers: config.ibus_triggers.clone(),
        engines : config.ibus_engines.clone(),
        link    : link.clone(),
        events  : events,
    });

    (Some(link), Some(receiver))
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
    /// Commands into the streaming recogniser thread.
    asr        : std::sync::mpsc::Sender<StreamCmd>,
    /// Commands into the offline recogniser thread.
    offline    : std::sync::mpsc::Sender<OfflineCmd>,
    /// Results out of both recogniser threads.
    asr_events : mpsc::UnboundedReceiver<AsrEvent>,
    /// Wayland text output, over whichever path the focus and the config
    /// allow.
    sink       : TextSink,
    /// The command link into the input-method thread, for the requests that
    /// are not text: an engine switch, a menu activation. `None` without the
    /// multiplexer. The sink holds its own clone for the text.
    im_link    : Option<ImLink>,
    /// What the input-method thread has told us, folded into the one state the
    /// popup renders. Owned here rather than in the applet so that mirrors get
    /// it through the snapshot like everything else.
    im_state   : InputMethodState,
    /// The input-method thread's events, taken by [`Engine::run`] so the
    /// select loop can hold them without borrowing `self` twice.
    im_events  : Option<mpsc::UnboundedReceiver<ImEvent>>,
    /// Trailing-silence detector for press-once mode.
    vad        : SilenceGate,
    /// Pause detector that cuts a long utterance into decodable segments.
    seg_gate   : SegmentGate,
    /// State transitions published to the applet.
    events     : broadcast::Sender<Event>,
    /// Most recent state event, replayed to mirrors when they subscribe.
    latest     : crate::ipc::Latest,

    /// Ring position where the current segment starts. The first segment of
    /// an utterance starts at `now - preroll`; each later one starts where its
    /// predecessor was cut.
    seg_start  : u64,
    /// Ring position up to which audio has been fed to the streaming model.
    cursor     : u64,
    /// Identifies the utterance segments belong to. Bumped when an utterance
    /// is abandoned, which is how results for it are recognised and dropped.
    utt_seq    : u64,
    /// Finished segment transcripts, in order. Joined at commit.
    seg_texts  : Vec<String>,
    /// Segments handed to the offline model that have not answered yet.
    seg_wait   : u32,
    /// Whether the utterance's last segment has been dispatched.
    seg_last   : bool,
    /// First segment error of this utterance, reported instead of a commit.
    seg_error  : Option<String>,
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
    /// Samples handed to the offline model across every segment, for the log.
    utt_len    : usize,
}

impl Engine {
    /// Builds an engine: starts capture, the recogniser thread, and the
    /// injector. Model loading continues in the background; the engine is
    /// usable once [`AsrEvent::Ready`] arrives.
    fn create(config: Config, events: broadcast::Sender<Event>) -> Result<Self> {
        let capture = Capture::start(config.preroll_ms).context("starting audio capture")?;
        let (link, im_events) = start_input_method(&config);
        let sink = TextSink::connect(&config, link.clone())?;
        tracing::info!("text output ready: {}", sink.status());
        let asr = crate::asr::spawn(&config);

        let im_state = match config.input_method {
            InputMethod::Off         => InputMethodState::Off,
            InputMethod::Multiplexer => InputMethodState::Stopped {
                reason: "starting up".to_owned(),
            },
        };
        let snapshot = crate::ipc::Snapshot {
            state        : None,
            logging      : config.log_transcripts,
            trigger      : config.trigger_code,
            input_method : im_state.clone(),
        };

        Ok(Self {
            vad        : SilenceGate::new(config.silence_ms),
            seg_gate   : SegmentGate::new(config.segment_pause_ms, config.min_segment_ms),
            state      : State::Idle,
            enabled    : true,
            logging    : config.log_transcripts,
            rebinding  : false,
            hotkey     : None,
            seg_start  : 0,
            cursor     : 0,
            utt_seq    : 0,
            seg_texts  : Vec::new(),
            seg_wait   : 0,
            seg_last   : false,
            seg_error  : None,
            started    : Instant::now(),
            last_sec   : 0,
            hyps       : VecDeque::new(),
            vk_typed   : String::new(),
            last_hyp   : String::new(),
            utt_len    : 0,
            config     : config,
            capture    : capture,
            asr        : asr.stream,
            offline    : asr.offline,
            asr_events : asr.events,
            sink       : sink,
            im_link    : link,
            im_state   : im_state,
            im_events  : im_events,
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

        // Out of `self` for the duration of the loop: `select!` builds every
        // arm's future at once, and two of them cannot both borrow `self`.
        let mut im_events = self.im_events.take();

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
                // `pending()` when there is no input-method thread, so the arm
                // simply never fires rather than needing a guard that would
                // borrow `self` a second time.
                evt = async {
                    match im_events.as_mut() {
                        Some(events) => events.recv().await,
                        None         => std::future::pending().await,
                    }
                } => match evt {
                    Some(evt) => self.on_im_event(evt),
                    // The supervisor thread is gone, which it only is if the
                    // process is shutting down.
                    None      => im_events = None,
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
        // The input-method requests are independent of the capture cycle and
        // of whether dictation is armed: switching engines with the trigger
        // disarmed is exactly what "disarm, keep typing" means.
        match cmd {
            Command::Enable        => return self.set_enabled(true),
            Command::Disable       => return self.set_enabled(false),
            Command::SetLogging(on) => return self.set_logging(on),
            Command::Rebind        => return self.start_rebind(),
            Command::SetEngine(name) => return self.send_im(ImCmd::SetEngine(name)),
            Command::ActivateProperty { key, state } => {
                return self.send_im(ImCmd::ActivateProperty {
                    key  : key,
                    state: state,
                });
            }
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
        // Segments still in the offline model are dropped on arrival because
        // the sequence has moved on; see `on_segment`.
        self.sink.cancel();
        self.abandon_utterance();
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

    /// Hands one request to the input-method thread, if there is one.
    fn send_im(&self, command: ImCmd) {
        match self.im_link.as_ref() {
            Some(link) => link.send(command),
            None       => tracing::warn!("ignoring {command:?}: the multiplexer is off"),
        }
    }

    /// Folds one input-method event into the state the popup renders.
    ///
    /// The producers say different things about the same subject — the
    /// switcher knows which engine is in effect and which ones the cycle
    /// holds, the frontend holds the engine's menu, the supervisor knows
    /// whether a frontend is running — so the engine keeps one state and each
    /// event updates the part it knows about. An `EngineChanged` while blocked,
    /// for instance, cannot happen, but an `EngineChanged` arriving one event
    /// before `Bound` can, and the engine is the only place that can hold all
    /// the pieces.
    fn on_im_event(&mut self, event: ImEvent) {
        // Whatever was already known about the running state carries over,
        // so that a menu update does not blank the engine name or a switch
        // forget the cycle.
        let (mut engine, mut engines, mut indicator, mut menu) = match &self.im_state {
            InputMethodState::Running { engine, engines, indicator, menu } => (
                engine.clone(),
                engines.clone(),
                indicator.clone(),
                menu.clone(),
            ),
            _ => (None, Vec::new(), None, Vec::new()),
        };
        let state = match event {
            ImEvent::EngineChanged(changed) => {
                engine = Some(changed);
                InputMethodState::Running {
                    engine   : engine,
                    engines  : engines,
                    indicator: indicator,
                    menu     : menu,
                }
            }
            ImEvent::Engines(cycle) => {
                engines = cycle;
                InputMethodState::Running {
                    engine   : engine,
                    engines  : engines,
                    indicator: indicator,
                    menu     : menu,
                }
            }
            ImEvent::Properties { indicator: glyph, menu: entries } => {
                indicator = glyph;
                menu = entries;
                InputMethodState::Running {
                    engine   : engine,
                    engines  : engines,
                    indicator: indicator,
                    menu     : menu,
                }
            }
            // Binding says nothing about the daemon, so everything known
            // about it is kept: losing the name would blank the status line
            // for as long as the engine stays unchanged.
            ImEvent::Bound => InputMethodState::Running {
                engine   : engine,
                engines  : engines,
                indicator: indicator,
                menu     : menu,
            },
            ImEvent::Blocked { reason } => InputMethodState::Blocked { reason: reason },
            ImEvent::Stopped { reason } => InputMethodState::Stopped { reason: reason },
        };

        if state == self.im_state {
            return;
        }
        self.im_state = state.clone();
        self.emit(Event::InputMethod { state: state });
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
        self.abandon_utterance();
        self.seg_start = self.capture.mark_preroll();
        self.cursor = self.seg_start;
        self.started = Instant::now();
        self.last_sec = 0;
        self.utt_len = 0;
        self.vad.reset();
        self.seg_gate.reset();
        self.hyps.clear();
        self.vk_typed.clear();
        self.last_hyp.clear();
        let _ = self.asr.send(StreamCmd::Reset);
        // Before the first partial, so the input method has finished whatever
        // conversion the keyboard left open and reset its engine by the time
        // one arrives. On the virtual-keyboard path this is a no-op.
        self.sink.begin();

        self.state = State::Recording { held: held };
        self.emit(Event::Recording { elapsed_ms: 0 });
    }

    /// Drops whatever the previous utterance accumulated.
    ///
    /// Bumping the sequence is what makes in-flight segment decodes harmless:
    /// their results arrive stamped with the old number and are discarded on
    /// arrival, so nothing has to be cancelled inside the recogniser.
    fn abandon_utterance(&mut self) {
        self.utt_seq += 1;
        self.seg_texts.clear();
        self.seg_wait = 0;
        self.seg_last = false;
        self.seg_error = None;
    }

    /// Drains new audio to the streaming model and enforces the end rules.
    fn on_tick(&mut self) {
        let State::Recording { held } = self.state else { return };

        let now = self.capture.now();
        if now > self.cursor {
            let chunk = self.capture.read(self.cursor, now);
            self.cursor = now;

            // Both gates see every window: the segment gate has to keep
            // counting speech even on the windows the end gate ignores.
            let boundary = self.seg_gate.push(&chunk);

            // Silence only ends the utterance when no key is holding it open;
            // in hold mode the human's finger is the endpointer.
            if self.vad.push(&chunk) && !held {
                self.finish_recording();
                return;
            }
            let _ = self.asr.send(StreamCmd::Feed(chunk));

            // The cut lands inside the pause the gate just measured, so no
            // word is split and the tail left for release stays short.
            if boundary {
                self.cut_segment(self.cursor, false);
            }
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

    /// Ends capture and hands the last segment to the offline model.
    ///
    /// Everything before the last pause is already decoded or decoding, so
    /// what the user waits on here is the tail, not the recording.
    fn finish_recording(&mut self) {
        self.cut_segment(self.capture.now(), true);

        self.state = State::Transcribing;
        self.emit(Event::Transcribing);
    }

    /// Sends `seg_start..end` to the offline model as one segment.
    ///
    /// Hotwords go with every segment: biasing has to apply wherever the word
    /// happens to fall, and the context graph is rebuilt per stream anyway.
    fn cut_segment(&mut self, end: u64, last: bool) {
        let samples = self.capture.read(self.seg_start, end);
        let hotwords = self.config.default_hotwords.clone();
        self.seg_start = end;
        self.utt_len += samples.len();
        self.seg_wait += 1;
        self.seg_last = last;

        let _ = self.offline.send(OfflineCmd::Transcribe {
            seq      : self.utt_seq,
            samples  : samples,
            hotwords : hotwords,
        });
    }

    /// Discards the in-flight utterance.
    fn cancel(&mut self) {
        // Clear any preedit the partials put up. Best effort; the compositor
        // clears it anyway when focus moves.
        self.sink.cancel();

        self.abandon_utterance();
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
                self.emit(Event::InputMethod { state: self.im_state.clone() });
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
            AsrEvent::Final { seq, text } => self.on_segment(seq, text),
        }
    }

    /// Files one segment's transcript, committing once the utterance is whole.
    ///
    /// Segments come back in dispatch order because the offline worker is a
    /// single thread, so appending is enough to reassemble the utterance.
    fn on_segment(&mut self, seq: u64, text: Result<String, String>) {
        // A cancelled, disarmed or superseded utterance: nobody is waiting.
        if seq != self.utt_seq {
            return;
        }
        self.seg_wait = self.seg_wait.saturating_sub(1);

        match text {
            Ok(text) => {
                if !text.is_empty() {
                    self.seg_texts.push(text);
                }
            }
            // Reported once, after the rest of the utterance settles: failing
            // a recording the user is still speaking into helps nobody.
            Err(reason) => {
                self.seg_error.get_or_insert(reason);
            }
        }
        if !self.seg_last || self.seg_wait > 0 {
            return;
        }

        if let Some(reason) = self.seg_error.take() {
            self.abandon_utterance();
            self.state = State::Idle;
            self.emit(Event::Failed { reason: reason });
            self.emit(Event::Idle);
            return;
        }

        // Segments are cut at pauses, so a space is the right joint: the model
        // has already punctuated each one as a sentence would end.
        let text = self.seg_texts.join(" ");
        let text = match choose_final(&text, &self.last_hyp) {
            Final::Offline          => text,
            Final::Streamed(reason) => {
                tracing::info!("committing the streamed text: {reason}");
                self.last_hyp.clone()
            }
        };
        self.abandon_utterance();
        self.commit(&text);
    }

    /// Routes provisional text to whichever display the focus allows.
    fn show_partial(&mut self, text: &str) {
        // Preedit first: free revision, replaced wholesale at commit.
        match self.sink.preedit(text) {
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
            if let Err(e) = self.sink.type_text(tail) {
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
            self.sink.cancel();
            self.state = State::Idle;
            self.emit(Event::Idle);
            return;
        }

        let mut out = text.to_owned();
        if self.config.trailing_space && wants_trailing_space(text) {
            out.push(' ');
        }

        let injected = match self.sink.commit(&out) {
            Ok(true)  => Ok(()),
            Ok(false) => {
                // Virtual-keyboard path: emit whatever the stable prefix has
                // not already typed. Divergence keeps the streamed text.
                match out.strip_prefix(self.vk_typed.as_str()) {
                    Some(tail) => self.sink.type_text(tail),
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
            Event::InputMethod { state } => {
                self.latest.lock().unwrap().input_method = state.clone();
            }
            _ => self.latest.lock().unwrap().state = Some(event.clone()),
        }
        let _ = self.events.send(event);
    }
}

// --- Which text to commit ---

/// The verdict of [`choose_final`].
#[derive(Debug, PartialEq, Eq)]
enum Final {
    /// The offline pass's text, as designed.
    Offline,
    /// The streaming model's last hypothesis, with the reason it won.
    Streamed(&'static str),
}

/// Decides between the offline pass and the streaming hypothesis.
///
/// The offline model is the accurate one and wins whenever it has an answer
/// in the script the user spoke. The streaming model is the multilingual one:
/// `nemotron-3.5-asr-streaming` writes Japanese, `parakeet-unified-en` has no
/// CJK token at all, so on Japanese speech the offline pass returns nothing
/// — or, worse, an English rendering of the sounds. Both cases are read here
/// as "the offline model cannot write what was said", and the streaming text
/// that the user has already been watching as preedit is committed instead.
/// Two empty strings are silence, and stay the caller's problem.
fn choose_final(offline: &str, streamed: &str) -> Final {
    if streamed.trim().is_empty() {
        return Final::Offline;
    }
    if offline.trim().is_empty() {
        return Final::Streamed("the offline pass heard nothing where the streaming model heard text");
    }
    if has_cjk(streamed) && !has_cjk(offline) {
        return Final::Streamed("the offline model cannot write the script the streaming model heard");
    }

    Final::Offline
}

/// Whether the text contains Han, kana or Hangul.
fn has_cjk(text: &str) -> bool {
    text.chars().any(is_cjk)
}

/// Han, hiragana, katakana (including half-width), Hangul, and the CJK
/// punctuation block. What "Japanese, Chinese or Korean" means for the two
/// rules that need it.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3000}'..='\u{303F}'   // CJK symbols and punctuation
        | '\u{3040}'..='\u{30FF}' // hiragana, katakana
        | '\u{3400}'..='\u{4DBF}' // CJK extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK unified ideographs
        | '\u{AC00}'..='\u{D7AF}' // Hangul syllables
        | '\u{F900}'..='\u{FAFF}' // CJK compatibility ideographs
        | '\u{FF00}'..='\u{FFEF}' // half-width and full-width forms
    )
}

/// Whether a committed utterance should get the configured trailing space.
///
/// Spaces separate words in Latin text and nothing in Japanese, where a space
/// after 。 is a typo. Decided by the last character, which is the one the
/// space would follow.
fn wants_trailing_space(text: &str) -> bool {
    text.chars().next_back().is_none_or(|last| !is_cjk(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// English as designed: the offline pass is the accurate one.
    #[test]
    fn offline_wins_when_it_has_an_answer() {
        assert_eq!(choose_final("The engine runs.", "the engine runs"), Final::Offline);
    }

    /// Silence stays silence; the streamed text does not resurrect nothing.
    #[test]
    fn two_empties_are_offline() {
        assert_eq!(choose_final("", ""), Final::Offline);
        assert_eq!(choose_final("", "  "), Final::Offline);
    }

    /// Japanese: the offline model has no token for it and returns nothing.
    #[test]
    fn streamed_wins_when_offline_is_empty() {
        assert!(matches!(choose_final("", "こんにちは"), Final::Streamed(_)));
    }

    /// Japanese rendered as English sounds is still not an answer.
    #[test]
    fn streamed_wins_when_offline_cannot_write_the_script() {
        assert!(matches!(choose_final("con each ewa", "こんにちは"), Final::Streamed(_)));
        // Mixed input where the offline pass did write CJK is its call.
        assert_eq!(choose_final("東京 station", "東京ステーション"), Final::Offline);
    }

    /// No space after Japanese; a space after everything else.
    #[test]
    fn trailing_space_follows_the_script() {
        assert!(wants_trailing_space("Hello."));
        assert!(wants_trailing_space(""));
        assert!(!wants_trailing_space("こんにちは。"));
        assert!(!wants_trailing_space("東京"));
        assert!(wants_trailing_space("東京 station"));
    }
}
