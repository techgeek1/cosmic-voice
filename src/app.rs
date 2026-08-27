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
        self, Alignment, Length, Limits, Subscription, window,
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
    },
    widget::{self, button, radio, settings, text, toggler},
};

use crate::config::Config;
use crate::engine::{self, Handle};
use crate::hotkey::key_name;
use crate::ipc::{Command, Event, ImEngine, ImPropKind, ImPropState, ImProperty, InputMethodState};

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
    /// Switch to the engine at this index of the cycle. An index rather than
    /// a name because iced's radio wants a `Copy` value.
    SetEngine(usize),
    /// Activate one entry of the engine's status menu.
    ActivateProperty {
        /// The entry's key.
        key  : String,
        /// The state to send, as the engine counts them.
        state: u32,
    },
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
            Message::SetEngine(index) => {
                if let (Some(handle), Some(engine)) = (ENGINE.get(), self.engine_cycle().get(index)) {
                    let _ = handle.commands.try_send(Command::SetEngine(engine.name.clone()));
                }
            }
            Message::ActivateProperty { key, state } => {
                if let Some(handle) = ENGINE.get() {
                    let _ = handle.commands.try_send(Command::ActivateProperty {
                        key  : key,
                        state: state,
                    });
                }
            }
        }

        cosmic::iced::Task::none()
    }

    /// The panel button: the microphone, and the input mode beside it.
    ///
    /// The glyph is what `ibus-ui-gtk3`'s tray icon used to show — mozc's
    /// あ / ア / A as the mode changes — and it lives on this button rather
    /// than in a tray item of its own because there is no longer any process
    /// whose job that is. Sized and padded like the applet's own icon button,
    /// so the panel does not grow when the glyph appears.
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

        let Some(glyph) = self.indicator() else {
            return self
                .core
                .applet
                .icon_button(name)
                .on_press_down(Message::TogglePopup)
                .into();
        };

        let (width, _) = self.core.applet.suggested_size(true);
        let (major, minor) = self.core.applet.suggested_padding(true);
        let icon = widget::icon::from_name(name).symbolic(true).size(width);
        let glyph = self.core.applet.text(glyph);
        let content: Element<'_, Message> = if self.core.applet.is_horizontal() {
            widget::row::with_capacity(2)
                .push(icon)
                .push(glyph)
                .spacing(minor)
                .align_y(Alignment::Center)
                .into()
        } else {
            widget::column::with_capacity(2)
                .push(icon)
                .push(glyph)
                .spacing(minor)
                .align_x(Alignment::Center)
                .into()
        };
        let padding = if self.core.applet.is_horizontal() { [minor, major] } else { [major, minor] };

        button::custom(content)
            .padding(padding)
            .class(cosmic::theme::Button::AppletIcon)
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

        let mut content = cosmic::widget::column::with_capacity(6)
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
        for section in self.input_method_sections() {
            content = content.push(padded_control(section));
        }

        self.core.applet.popup_container(content).into()
    }
}

// --- The input-method section ---

impl App {
    /// The engines the switch hotkey cycles through, or nothing while the
    /// multiplexer is not running.
    fn engine_cycle(&self) -> &[ImEngine] {
        match &self.input_method {
            InputMethodState::Running { engines, .. } => engines,
            _                                         => &[],
        }
    }

    /// The live mode glyph, only while the multiplexer is running.
    fn indicator(&self) -> Option<&str> {
        match &self.input_method {
            InputMethodState::Running { indicator, .. } => indicator.as_deref(),
            _                                           => None,
        }
    }

    /// The "Input method" part of the popup: the engine cycle, then the
    /// engine's own menu.
    ///
    /// Sections rather than one list because that is the shape the menu has:
    /// the engine cycle is one group of choices, and every `Menu` property —
    /// mozc's input modes, mozc's tools — is another with a title of its
    /// own. Everything that is not a menu sits in the first section under
    /// the engines. Empty while there is nothing to show, so a running
    /// multiplexer with an xkb engine and nothing configured to cycle to
    /// adds nothing to the popup.
    fn input_method_sections(&self) -> Vec<Element<'_, Message>> {
        let InputMethodState::Running { engine, engines, menu, .. } = &self.input_method else {
            return Vec::new();
        };
        let mut sections = Vec::new();

        let current = engine
            .as_ref()
            .and_then(|current| engines.iter().position(|entry| entry.name == current.name));
        let mut top = settings::section().title("Input method");
        let mut has_rows = false;
        for (index, entry) in engines.iter().enumerate() {
            top = top.add(
                radio(text::body(entry.to_string()), index, current, Message::SetEngine)
                    .width(Length::Fill),
            );
            has_rows = true;
        }

        // The menu itself. A `Menu` becomes a section; everything else at
        // the top level joins the engine section.
        let mut menus = Vec::new();
        for property in menu.iter().filter(|property| property.visible) {
            match property.kind {
                ImPropKind::Menu => menus.push(property),
                _                => {
                    if let Some(row) = menu_row(property, None) {
                        top = top.add(row);
                        has_rows = true;
                    }
                }
            }
        }
        if has_rows {
            sections.push(top.into());
        }
        for property in menus {
            if let Some(section) = menu_section(property) {
                sections.push(section);
            }
        }

        sections
    }
}

/// One `Menu` property as a titled section of its entries.
///
/// Radio children share one selection, which is why they are drawn here
/// with the group in hand rather than one at a time: iced's radio wants to
/// know which sibling is checked. A menu nested inside a menu is flattened
/// into its parent's section under a heading, since a popup has no room for
/// submenus and the engines that exist do not nest.
fn menu_section(property: &ImProperty) -> Option<Element<'_, Message>> {
    let mut section = settings::section().title(property.label.clone());
    let mut rows = 0;
    let checked = property
        .children
        .iter()
        .filter(|child| child.visible)
        .position(|child| child.kind == ImPropKind::Radio && child.state == ImPropState::Checked);

    for (index, child) in property.children.iter().filter(|child| child.visible).enumerate() {
        match child.kind {
            ImPropKind::Menu => {
                section = section.add(text::caption_heading(child.label.clone()));
                for grandchild in child.children.iter().filter(|child| child.visible) {
                    if let Some(row) = menu_row(grandchild, None) {
                        section = section.add(row);
                        rows += 1;
                    }
                }
            }
            _ => {
                if let Some(row) = menu_row(child, Some((index, checked))) {
                    section = section.add(row);
                    rows += 1;
                }
            }
        }
    }

    if rows == 0 { None } else { Some(section.into()) }
}

/// One non-menu property as a popup row.
///
/// `group` is the row's index within its radio group and the group's checked
/// index, for a radio; a radio outside any group is drawn as a button, since
/// there is nothing for it to be exclusive with. An insensitive entry keeps
/// its row and loses its message, which is what a greyed-out menu item is —
/// for a radio that means a button with no press, because iced's radio has
/// no disabled form.
fn menu_row(
    property: &ImProperty,
    group   : Option<(usize, Option<usize>)>,
) -> Option<Element<'_, Message>> {
    let key = property.key.clone();
    let row: Element<'_, Message> = match property.kind {
        ImPropKind::Separator => widget::divider::horizontal::light().into(),
        ImPropKind::Toggle => {
            let checked = property.state == ImPropState::Checked;
            let toggle = toggler(checked).on_toggle_maybe(property.sensitive.then_some(
                move |on: bool| Message::ActivateProperty {
                    key  : key.clone(),
                    state: ImPropState::from_bool(on).as_u32(),
                },
            ));
            settings::item(property.label.clone(), toggle).into()
        }
        ImPropKind::Radio if group.is_some() && property.sensitive => {
            let (index, checked) = group.expect("matched on is_some");
            // Checked, not the row's own state: mozc acts on an input mode
            // only when the activation says checked, and a radio that is
            // pressed is being chosen.
            radio(text::body(property.label.clone()), index, checked, move |_| {
                Message::ActivateProperty {
                    key  : key,
                    state: ImPropState::Checked.as_u32(),
                }
            })
            .width(Length::Fill)
            .into()
        }
        ImPropKind::Normal | ImPropKind::Radio | ImPropKind::Menu => {
            let message = property.sensitive.then(|| Message::ActivateProperty {
                key  : key,
                state: ImPropState::Checked.as_u32(),
            });
            button::text(property.label.clone())
                .on_press_maybe(message)
                .width(Length::Fill)
                .into()
        }
    };

    Some(row)
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
    /// The glyph is the live indicator when the engine has one — mozc's
    /// current mode — and the engine's static symbol otherwise.
    fn engine_label(&self) -> Option<String> {
        let InputMethodState::Running { engine, indicator, .. } = &self.input_method else {
            return None;
        };
        let engine = engine.as_ref()?;
        let name = if engine.longname.is_empty() { &engine.name } else { &engine.longname };
        let glyph = indicator.as_deref().unwrap_or(&engine.symbol);

        Some(if glyph.is_empty() { name.clone() } else { format!("{name} {glyph}") })
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
            InputMethodState::Running { engine: None, .. } => {
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
