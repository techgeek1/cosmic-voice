//! The panel applet: an icon reflecting engine state, and a small popup.
//!
//! Deliberately thin. The engine owns the pipeline on its own thread and this
//! is one subscriber to its event stream; closing the popup, or the panel
//! restarting the applet process, must never interrupt a dictation in
//! progress any more than it can avoid.

use std::sync::OnceLock;

use cosmic::{
    Element,
    app,
    applet::padded_control,
    iced::{
        self, Limits, Subscription, window,
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
    },
    widget::{button, settings, text, toggler},
};

use crate::config::Config;
use crate::engine::{self, Handle};
use crate::hotkey::key_name;
use crate::ipc::{Command, Event, InputMethodState};

const APP_ID: &str = "dev.techgeek1.CosmicExtAppletVoice";
const POPUP_MIN_WIDTH: f32 = 300.0;
const POPUP_MAX_WIDTH: f32 = 372.0;

/// The engine handle, global so the event subscription (a plain `fn`) can
/// reach it. Set once in `init`.
static ENGINE: OnceLock<Handle> = OnceLock::new();

/// Runs the applet. Blocks for the process lifetime.
pub fn run() -> anyhow::Result<()> {
    cosmic::applet::run::<App>(())?;

    Ok(())
}

// --- App ---

/// What the panel needs to render: a digest of the engine's event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineState {
    /// Models still loading.
    Starting,
    /// Ready and quiet.
    Idle,
    /// Capturing audio.
    Recording,
    /// Offline model running.
    Transcribing,
    /// Something failed; details in `App::error`.
    Failed,
    /// Trigger disarmed by the user.
    Disabled,
    /// Waiting for the user to press the new trigger key.
    Rebinding,
}

pub struct App {
    core        : cosmic::app::Core,
    popup       : Option<window::Id>,
    state       : EngineState,
    /// Elapsed seconds while recording, for the popup.
    elapsed_s   : u64,
    /// Last failure, cleared on the next successful cycle.
    error       : Option<String>,
    /// Whether the engine is appending transcripts to the corpus log.
    logging     : bool,
    /// evdev code of the trigger key in effect.
    trigger     : u16,
    /// Where the input-method multiplexer is. Orthogonal to `state`: dictation
    /// works in every one of these, only the path it takes changes.
    input_method: InputMethodState,
}

#[derive(Clone, Debug)]
pub enum Message {
    TogglePopup,
    PopupClosed(window::Id),
    Engine(Event),
    SetEnabled(bool),
    SetLogging(bool),
    Rebind,
}

impl cosmic::Application for App {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn init(core: cosmic::app::Core, _flags: ()) -> (Self, app::Task<Message>) {
        let _ = ENGINE.get_or_init(|| engine::spawn(Config::load()));

        let app = App {
            core        : core,
            popup       : None,
            state       : EngineState::Starting,
            elapsed_s   : 0,
            error       : None,
            logging     : false,
            trigger     : 0,
            input_method: InputMethodState::Off,
        };

        (app, cosmic::iced::Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::run(engine_stream)
    }

    fn update(&mut self, message: Message) -> app::Task<Message> {
        match message {
            Message::TogglePopup => {
                if let Some(popup) = self.popup.take() {
                    return destroy_popup(popup);
                }
                let id = window::Id::unique();
                self.popup.replace(id);
                let mut settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    id,
                    None,
                    None,
                    None,
                );
                settings.positioner.size_limits = Limits::NONE
                    .min_width(POPUP_MIN_WIDTH)
                    .max_width(POPUP_MAX_WIDTH)
                    .min_height(1.0)
                    .max_height(1080.0);
                return get_popup(settings);
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::Engine(event) => self.apply(event),
            Message::SetEnabled(enabled) => {
                if let Some(handle) = ENGINE.get() {
                    let cmd = if enabled { Command::Enable } else { Command::Disable };
                    let _ = handle.commands.try_send(cmd);
                }
            }
            Message::SetLogging(enabled) => {
                if let Some(handle) = ENGINE.get() {
                    let _ = handle.commands.try_send(Command::SetLogging(enabled));
                }
            }
            Message::Rebind => {
                if let Some(handle) = ENGINE.get() {
                    let _ = handle.commands.try_send(Command::Rebind);
                }
            }
        }

        cosmic::iced::Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let name = match self.state {
            EngineState::Starting     => "content-loading-symbolic",
            EngineState::Idle         => "audio-input-microphone-symbolic",
            EngineState::Recording    => "media-record-symbolic",
            EngineState::Transcribing => "emblem-synchronizing-symbolic",
            EngineState::Failed       => "dialog-error-symbolic",
            EngineState::Disabled     => "microphone-disabled-symbolic",
            EngineState::Rebinding    => "input-keyboard-symbolic",
        };
        // An input method that refuses to bind is invisible otherwise — the
        // preedit simply never appears — so it takes the icon over from the
        // states that are not themselves saying something more urgent.
        let name = match (&self.input_method, self.state) {
            (InputMethodState::Blocked { .. }, EngineState::Idle | EngineState::Starting) => {
                "dialog-warning-symbolic"
            }
            _ => name,
        };

        self.core
            .applet
            .icon_button(name)
            .on_press_down(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let status = match self.state {
            EngineState::Starting     => "Loading models…".to_owned(),
            EngineState::Idle         => match self.engine_label() {
                Some(engine) => {
                    format!("Ready — hold {} to talk · {engine}", key_name(self.trigger))
                }
                None => format!("Ready — hold {} to talk", key_name(self.trigger)),
            },
            EngineState::Recording    => format!("Recording… {}s", self.elapsed_s),
            EngineState::Transcribing => "Transcribing…".to_owned(),
            EngineState::Failed       => "Failed".to_owned(),
            EngineState::Disabled     => "Disabled — trigger inactive".to_owned(),
            EngineState::Rebinding    => "Press the new trigger key (Esc cancels)".to_owned(),
        };

        let enabled = self.state != EngineState::Disabled;
        let rebind = if self.state == EngineState::Rebinding {
            button::standard("Press a key…")
        } else {
            button::standard(key_name(self.trigger)).on_press(Message::Rebind)
        };
        let controls = settings::section()
            .add(settings::item(
                "Dictation",
                toggler(enabled).on_toggle(Message::SetEnabled),
            ))
            .add(settings::item(
                "Log transcripts",
                toggler(self.logging).on_toggle(Message::SetLogging),
            ))
            .add(settings::item("Trigger key", rebind));

        let mut content = cosmic::widget::column::with_capacity(4)
            .padding([8, 0])
            .spacing(8)
            .push(padded_control(text::heading(status)));
        if let Some(error) = &self.error {
            content = content.push(padded_control(text::body(error.clone())));
        }
        if let Some(line) = self.input_method_line() {
            content = content.push(padded_control(text::body(line)));
        }
        content = content.push(padded_control(controls));

        self.core.applet.popup_container(content).into()
    }
}

impl App {
    /// Folds one engine event into the render state.
    fn apply(&mut self, event: Event) {
        match event {
            Event::Idle => {
                if self.state != EngineState::Failed {
                    self.state = EngineState::Idle;
                }
            }
            Event::Recording { elapsed_ms } => {
                self.state = EngineState::Recording;
                self.error = None;
                self.elapsed_s = elapsed_ms / 1000;
            }
            Event::Transcribing => {
                self.state = EngineState::Transcribing;
            }
            Event::Partial { .. } => {}
            Event::Injected { .. } => {
                self.state = EngineState::Idle;
                self.error = None;
            }
            Event::Failed { reason } => {
                self.state = EngineState::Failed;
                self.error = Some(reason);
            }
            Event::Disabled => {
                self.state = EngineState::Disabled;
            }
            Event::Logging { enabled } => {
                self.logging = enabled;
            }
            Event::Rebinding => {
                self.state = EngineState::Rebinding;
                self.error = None;
            }
            Event::Trigger { code } => {
                self.trigger = code;
            }
            Event::InputMethod { state } => {
                self.input_method = state;
            }
        }
    }

    /// The engine name to hang off the ready line, e.g. `Mozc あ`.
    ///
    /// Only while the multiplexer is actually running: a name left over from
    /// before a frontend died would claim an input method that is not there.
    fn engine_label(&self) -> Option<String> {
        match &self.input_method {
            InputMethodState::Running { engine } => engine.as_ref().map(ToString::to_string),
            _                                    => None,
        }
    }

    /// The extra popup line for an input method that needs explaining.
    ///
    /// `None` for the two states that speak for themselves: switched off, and
    /// running with an engine the ready line already names.
    fn input_method_line(&self) -> Option<String> {
        match &self.input_method {
            InputMethodState::Off => None,
            InputMethodState::Blocked { reason } => {
                Some(format!("Input method: blocked — {reason}"))
            }
            InputMethodState::Stopped { reason } => {
                Some(format!("Input method: not running — {reason}"))
            }
            InputMethodState::Running { engine: None } => {
                Some("Input method: bound, waiting for IBus".to_owned())
            }
            InputMethodState::Running { .. } => None,
        }
    }
}

/// The engine's event stream, adapted for the iced subscription.
fn engine_stream() -> impl iced::futures::Stream<Item = Message> + Send {
    let receiver = ENGINE.get().map(|handle| handle.events.subscribe());

    iced::futures::stream::unfold(receiver, |mut receiver| async move {
        let rx = receiver.as_mut()?;
        loop {
            match rx.recv().await {
                Ok(event) => return Some((Message::Engine(event), receiver)),
                // Skipped a few under load; the next event carries fresh state.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}
