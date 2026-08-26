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
        self, Length, Subscription, window,
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
    },
    widget::{button, row, text},
};

use crate::config::Config;
use crate::engine::{self, Handle};
use crate::ipc::{Command, Event};

const APP_ID: &str = "dev.techgeek1.CosmicExtAppletVoice";
const POPUP_WIDTH: f32 = 300.0;

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
}

pub struct App {
    core       : cosmic::app::Core,
    popup      : Option<window::Id>,
    state      : EngineState,
    /// Elapsed seconds while recording, for the popup.
    elapsed_s  : u64,
    /// Live hypothesis while recording, shown in the popup.
    partial    : String,
    /// Last committed transcript.
    last_text  : Option<String>,
    /// Last failure, cleared on the next successful cycle.
    error      : Option<String>,
}

#[derive(Clone, Debug)]
pub enum Message {
    TogglePopup,
    PopupClosed(window::Id),
    Engine(Event),
    Toggle,
    Cancel,
    SetEnabled(bool),
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
            core      : core,
            popup     : None,
            state     : EngineState::Starting,
            elapsed_s : 0,
            partial   : String::new(),
            last_text : None,
            error     : None,
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
                let settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    id,
                    None,
                    None,
                    None,
                );
                return get_popup(settings);
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::Engine(event) => self.apply(event),
            Message::Toggle => {
                if let Some(handle) = ENGINE.get() {
                    let _ = handle.commands.try_send(Command::Toggle);
                }
            }
            Message::Cancel => {
                if let Some(handle) = ENGINE.get() {
                    let _ = handle.commands.try_send(Command::Cancel);
                }
            }
            Message::SetEnabled(enabled) => {
                if let Some(handle) = ENGINE.get() {
                    let cmd = if enabled { Command::Enable } else { Command::Disable };
                    let _ = handle.commands.try_send(cmd);
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
            EngineState::Idle         => "Ready — hold F13 to talk".to_owned(),
            EngineState::Recording    => format!("Recording… {}s", self.elapsed_s),
            EngineState::Transcribing => "Transcribing…".to_owned(),
            EngineState::Failed       => "Failed".to_owned(),
            EngineState::Disabled     => "Disabled — trigger inactive".to_owned(),
        };

        let mut content = cosmic::widget::column::with_capacity(4)
            .spacing(8)
            .push(padded_control(text::heading(status)));

        if self.state == EngineState::Recording && !self.partial.is_empty() {
            content = content.push(padded_control(text::body(self.partial.clone())));
        }
        if let Some(error) = &self.error {
            content = content.push(padded_control(text::body(error.clone())));
        }
        if let Some(last) = &self.last_text {
            content = content.push(padded_control(text::caption(last.clone())));
        }

        let controls = if self.state == EngineState::Disabled {
            row::with_capacity(1)
                .spacing(8)
                .push(button::standard("Enable").on_press(Message::SetEnabled(true)))
        } else {
            let toggle_label = if self.state == EngineState::Recording { "Stop" } else { "Start" };
            row::with_capacity(3)
                .spacing(8)
                .push(button::standard(toggle_label).on_press(Message::Toggle))
                .push(button::standard("Cancel").on_press(Message::Cancel))
                .push(button::standard("Disable").on_press(Message::SetEnabled(false)))
        };
        content = content.push(padded_control(controls));

        self.core
            .applet
            .popup_container(content.width(Length::Fixed(POPUP_WIDTH)))
            .into()
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
                self.partial.clear();
            }
            Event::Recording { elapsed_ms } => {
                self.state = EngineState::Recording;
                self.error = None;
                self.elapsed_s = elapsed_ms / 1000;
            }
            Event::Transcribing => {
                self.state = EngineState::Transcribing;
            }
            Event::Partial { text } => {
                self.partial = text;
            }
            Event::Injected { text } => {
                self.state = EngineState::Idle;
                self.error = None;
                self.last_text = Some(text);
                self.partial.clear();
            }
            Event::Failed { reason } => {
                self.state = EngineState::Failed;
                self.error = Some(reason);
            }
            Event::Disabled => {
                self.state = EngineState::Disabled;
                self.partial.clear();
            }
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
