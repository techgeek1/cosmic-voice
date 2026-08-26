//! Getting transcribed text into whatever window has focus.
//!
//! Two protocols, picked at runtime rather than configured.
//!
//! `zwp_input_method_v2` is the fast path: `set_preedit_string` shows live
//! provisional text the client renders as pending, replaced as often as we
//! like, and `commit_string` finalises it. This is the same mechanism a CJK
//! input method uses, revision is free, and the client owns the rendering. It
//! only works when a text-input-v3 capable client is focused *and* nothing
//! else holds the seat's single input-method slot — the compositor says
//! `unavailable` if ibus-wayland got there first, and `activate` only arrives
//! while a capable client has focus. Both states are events, so availability
//! is observed rather than guessed.
//!
//! `zwp_virtual_keyboard_v1` reaches everything, because it emits key events
//! at the seat. The cost is synthesising an XKB keymap containing exactly the
//! characters to be typed, uploading it as an fd, then sending press and
//! release per keycode. There is no provisional state on this path and
//! backspacing is destructive (the caret may have moved, a bracket may have
//! auto-closed), so it never revises: the engine only hands it text that is
//! final, or a stable prefix it is prepared to stand behind.
//!
//! The injector deliberately owns *mechanisms*, not policy: `preedit` and
//! `commit_im` report whether the IM path worked, `type_text` types verbatim,
//! and the engine decides what to send where. It also runs on its own Wayland
//! connection, separate from the one libcosmic runs the applet on, so the
//! input-method role's lifecycle stays untangled from the panel's.

use anyhow::{Context, Result};
use std::os::fd::AsFd;
use std::time::Instant;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

/// Distinct characters a single synthesised keymap can carry. XKB keycodes
/// run 8..=255 and we start at 9, so the true bound is 247; a little headroom
/// costs nothing.
const KEYMAP_BUDGET: usize = 200;

// --- Wayland state ---

/// Protocol state updated by the event queue.
struct State {
    /// Whether the compositor granted us the seat's input-method slot.
    /// `unavailable` flips this off permanently for this binding.
    im_alive     : bool,
    /// Double-buffered activation: set by `activate`/`deactivate`, applied on
    /// `done`.
    im_pending   : bool,
    /// Whether a text-input-capable client is focused right now.
    im_active    : bool,
    /// Count of `done` events, which is the serial `commit` must carry.
    im_serial    : u32,
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_input_method_v2::Event;
        match event {
            Event::Activate     => state.im_pending = true,
            Event::Deactivate   => state.im_pending = false,
            Event::Done         => {
                state.im_active = state.im_pending;
                state.im_serial += 1;
            }
            Event::Unavailable  => state.im_alive = false,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(State: ignore ZwpInputMethodManagerV2);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// --- Injector ---

/// Owns a Wayland connection dedicated to text injection.
pub struct Injector {
    /// Event queue for the dedicated connection.
    queue     : EventQueue<State>,
    /// Protocol state the queue updates.
    state     : State,
    /// The virtual keyboard, bound unconditionally.
    vk        : ZwpVirtualKeyboardV1,
    /// The input method, absent when the manager global is missing.
    im        : Option<ZwpInputMethodV2>,
    /// Timestamp origin for synthesised key events.
    epoch     : Instant,
    /// Whether the current preedit is non-empty, so `commit_im` knows to clear.
    preediting: bool,
}

impl Injector {
    /// Connects and binds the injection protocols.
    ///
    /// The input method is only bound when `bind_im` is set, and the default
    /// configuration leaves it off: binding the seat's single IM slot while
    /// IBus runs its Wayland IM wedged cosmic-comp's keyboard routing and
    /// killed all text input session-wide. With `bind_im` off this object is
    /// purely a virtual keyboard and contends with nothing.
    ///
    /// Failing to bind the virtual keyboard *is* fatal, because then there is
    /// no way to type at all.
    pub fn connect(bind_im: bool) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the compositor")?;
        let (globals, queue) = registry_queue_init::<State>(&conn)
            .context("initialising the registry")?;
        let qh = queue.handle();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=9, ())
            .context("binding wl_seat")?;
        let vk_mgr: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("binding zwp_virtual_keyboard_manager_v1")?;
        let vk = vk_mgr.create_virtual_keyboard(&seat, &qh, ());

        let im = if bind_im {
            globals
                .bind::<ZwpInputMethodManagerV2, _, _>(&qh, 1..=1, ())
                .ok()
                .map(|mgr| mgr.get_input_method(&seat, &qh, ()))
        } else {
            None
        };

        let mut this = Self {
            queue      : queue,
            state      : State { im_alive: im.is_some(), im_pending: false, im_active: false, im_serial: 0 },
            vk         : vk,
            im         : im,
            epoch      : Instant::now(),
            preediting : false,
        };
        // Collect the initial activate/done or unavailable before anyone asks.
        this.pump()?;

        Ok(this)
    }

    /// Processes pending events so the activation state is current.
    pub fn pump(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.state)
            .context("wayland roundtrip")?;

        Ok(())
    }

    /// Whether the preedit path can be used right now.
    ///
    /// True only while we hold the input-method slot *and* the focused client
    /// speaks text-input-v3. Answered from current events, so call sites see
    /// focus changes without polling anything themselves.
    pub fn im_usable(&mut self) -> bool {
        if self.pump().is_err() {
            return false;
        }

        self.im.is_some() && self.state.im_alive && self.state.im_active
    }

    /// Human-readable input-method state, for diagnostics.
    pub fn im_status(&self) -> &'static str {
        match (&self.im, self.state.im_alive, self.state.im_active) {
            (None, ..)           => "not bound",
            (_, false, _)        => "unavailable (slot held by another IME)",
            (_, true, false)     => "bound, inactive (no text-input client focused)",
            (_, true, true)      => "bound, active",
        }
    }

    /// Replaces the provisional text shown by the focused client.
    ///
    /// Returns false when the IM path is not usable; nothing was shown and the
    /// caller decides what the fallback policy is.
    pub fn preedit(&mut self, text: &str) -> Result<bool> {
        if !self.im_usable() {
            return Ok(false);
        }
        let im = self.im.as_ref().expect("im_usable implies im");

        let end = text.len() as i32;
        im.set_preedit_string(text.into(), end, end);
        im.commit(self.state.im_serial);
        self.preediting = !text.is_empty();
        self.pump()?;

        Ok(true)
    }

    /// Commits final text through the input method, clearing any preedit.
    ///
    /// Returns false when the IM path is not usable — including when focus
    /// moved to a non-text-input client mid-utterance, in which case any
    /// preedit was already discarded by the compositor and the caller should
    /// fall back to the virtual keyboard.
    pub fn commit_im(&mut self, text: &str) -> Result<bool> {
        if !self.im_usable() {
            return Ok(false);
        }
        let im = self.im.as_ref().expect("im_usable implies im");

        im.set_preedit_string(String::new(), 0, 0);
        im.commit_string(text.into());
        im.commit(self.state.im_serial);
        self.preediting = false;
        self.pump()?;

        Ok(true)
    }

    /// Types `text` verbatim through the virtual keyboard.
    ///
    /// Builds a keymap holding the text's distinct characters, uploads it,
    /// waits for the compositor to install it, then taps out the sequence.
    /// Text with more distinct characters than one keymap can hold is typed in
    /// segments with a keymap swap between them.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        for segment in segment_by_distinct(text, KEYMAP_BUDGET) {
            self.type_segment(&segment)?;
        }

        Ok(())
    }
}

impl Injector {
    /// Types one segment whose distinct characters fit a single keymap.
    fn type_segment(&mut self, text: &str) -> Result<()> {
        // Assign keycodes in order of first appearance. XKB keycode 9 is the
        // first we use; the wire protocol carries evdev codes, which are the
        // XKB code minus 8.
        let mut order: Vec<char> = Vec::new();
        for ch in text.chars() {
            if !order.contains(&ch) {
                order.push(ch);
            }
        }

        let keymap = build_keymap(&order);
        let fd = upload_keymap(&keymap)?;
        self.vk
            .keymap(1 /* XKB_KEYMAP_FORMAT_TEXT_V1 */, fd.as_fd(), keymap.len() as u32 + 1);
        // The keymap request carries no ack, so a roundtrip is the only way to
        // know the compositor has installed it before keys start arriving.
        self.pump()?;
        self.vk.modifiers(0, 0, 0, 0);

        for ch in text.chars() {
            let idx = order.iter().position(|&c| c == ch).expect("built from this text");
            let code = idx as u32 + 1; // evdev code; XKB sees +8 = 9.., matching the keymap
            let t = self.epoch.elapsed().as_millis() as u32;
            self.vk.key(t, code, 1);
            self.vk.key(t, code, 0);
        }
        self.pump()?;

        Ok(())
    }
}

// --- Keymap synthesis ---

/// Splits text into runs whose distinct-character count stays within `budget`.
fn segment_by_distinct(text: &str, budget: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut distinct: Vec<char> = Vec::new();

    for ch in text.chars() {
        if !distinct.contains(&ch) {
            if distinct.len() == budget {
                out.push(std::mem::take(&mut current));
                distinct.clear();
            }
            distinct.push(ch);
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }

    out
}

/// Renders an XKB keymap mapping keycode 9+n to the nth character.
fn build_keymap(chars: &[char]) -> String {
    use std::fmt::Write;

    let mut codes = String::new();
    let mut syms = String::new();
    for (i, &ch) in chars.iter().enumerate() {
        let code = i + 9;
        let _ = writeln!(codes, "\t\t<K{i}> = {code};");
        let _ = writeln!(syms, "\t\tkey <K{i}> {{ [ {} ] }};", keysym(ch));
    }

    format!(
        "xkb_keymap {{\n\
         \txkb_keycodes \"cosmic-voice\" {{\n\
         \t\tminimum = 8;\n\
         \t\tmaximum = 255;\n\
         {codes}\
         \t}};\n\
         \txkb_types \"cosmic-voice\" {{\n\
         \t\ttype \"ONE_LEVEL\" {{ modifiers = none; map[none] = Level1; }};\n\
         \t}};\n\
         \txkb_compatibility \"cosmic-voice\" {{ }};\n\
         \txkb_symbols \"cosmic-voice\" {{\n\
         {syms}\
         \t}};\n\
         }};\n"
    )
}

/// XKB keysym name for a character.
///
/// The `U<hex>` form covers all of Unicode; the control characters that would
/// otherwise map to nothing get their named keysyms.
fn keysym(ch: char) -> String {
    match ch {
        '\n' => "Return".into(),
        '\t' => "Tab".into(),
        _    => format!("U{:04X}", ch as u32),
    }
}

/// Puts the keymap text, NUL-terminated, into a sealed memfd.
fn upload_keymap(keymap: &str) -> Result<std::os::fd::OwnedFd> {
    use rustix::fs::{MemfdFlags, memfd_create};
    use std::io::Write;

    let fd = memfd_create("cosmic-voice-keymap", MemfdFlags::CLOEXEC)
        .context("memfd_create")?;
    let mut file = std::fs::File::from(fd);
    file.write_all(keymap.as_bytes()).context("writing keymap")?;
    file.write_all(&[0]).context("writing keymap terminator")?;

    Ok(file.into())
}

/// Longest common prefix of two strings on char boundaries.
///
/// Used by the engine to work out which part of a new hypothesis is already
/// typed and which tail still needs emitting on the virtual-keyboard path.
pub fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        len += ca.len_utf8();
    }

    len
}

/// Result of an injection, for logging and the applet's history line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `zwp_input_method_v2::commit_string`.
    InputMethod,
    /// Synthesised keymap over `zwp_virtual_keyboard_v1`.
    VirtualKeyboard,
}
