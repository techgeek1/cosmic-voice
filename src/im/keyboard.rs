//! The keymap half of the keyboard grab: what a keycode *means*.
//!
//! The grab hands us hardware: an evdev keycode and a modifier quadruplet.
//! IBus wants meaning: an X keysym and its own modifier bits. Between the two
//! sits an XKB keymap that the compositor sends us as a file descriptor, and
//! this module is the state built from it.
//!
//! Three numbering conventions meet here and they are all different, which is
//! the single most productive source of bugs in this area:
//!
//! - **Wayland** carries evdev codes (`a` is 30).
//! - **XKB** offsets them by 8 (`a` is 38), because X11 keycodes start at 8.
//! - **IBus** wants the XKB number — evdev+8 — as its `keycode` argument. The
//!   ecosystem is inconsistent about this, but the daemon passes the value
//!   through untouched and every engine is tested against GTK, which sends
//!   evdev+8. See `docs/multiplexer.md`.
//!
//! Everything below takes evdev codes at its edges and does the +8 itself, so
//! callers never hold a number whose convention is ambiguous.
//!
//! # Modifier state
//!
//! We track modifiers from the grab's `modifiers` event alone
//! (`xkb_state_update_mask`), never from `xkb_state_update_key`. IBus's own
//! bridge does both (`ibuswaylandim.c:1240-1241` against `:1708-1745`), which
//! is what xkbcommon's documentation tells clients not to do: the two are
//! different ways of driving the same latch and lock state machine, and
//! running both double-counts a latch. Every ordinary Wayland client uses the
//! mask path only, and the compositor sends a `modifiers` event before the key
//! event that changed them, so nothing is lost by dropping the other half.

use anyhow::{Context as _, Result};
use std::os::fd::OwnedFd;
use xkbcommon::xkb;

use crate::ibus::{
    CONTROL_MASK, HYPER_MASK, LOCK_MASK, META_MASK, MOD1_MASK, MOD2_MASK, MOD3_MASK, MOD4_MASK,
    MOD5_MASK, SHIFT_MASK, SUPER_MASK,
};

/// The offset between an evdev keycode and the XKB keycode for the same key.
pub const XKB_KEYCODE_OFFSET: u32 = 8;

// --- Modifier masks ---

/// The bit each named modifier occupies in *this* keymap.
///
/// The indices are keymap-specific — a layout that does not define `Mod5` at
/// all leaves it invalid — so they are recomputed on every keymap and a
/// missing modifier becomes an empty mask that can never match.
#[derive(Debug, Default, Clone, Copy)]
struct ModMasks {
    /// `Shift`.
    shift  : xkb::ModMask,
    /// `Lock`, which is Caps Lock.
    lock   : xkb::ModMask,
    /// `Control`.
    control: xkb::ModMask,
    /// `Mod1`, conventionally Alt.
    mod1   : xkb::ModMask,
    /// `Mod2`, conventionally Num Lock.
    mod2   : xkb::ModMask,
    /// `Mod3`.
    mod3   : xkb::ModMask,
    /// `Mod4`, conventionally Super.
    mod4   : xkb::ModMask,
    /// `Mod5`, conventionally `ISO_Level3_Shift`.
    mod5   : xkb::ModMask,
    /// `Super`, a virtual modifier that usually aliases `Mod4`.
    super_ : xkb::ModMask,
    /// `Hyper`, a virtual modifier.
    hyper  : xkb::ModMask,
    /// `Meta`, a virtual modifier.
    meta   : xkb::ModMask,
}

impl ModMasks {
    /// Reads the indices out of a freshly compiled keymap.
    fn from_keymap(keymap: &xkb::Keymap) -> Self {
        let mask = |name: &str| match keymap.mod_get_index(name) {
            xkb::MOD_INVALID => 0,
            index            => 1 << index,
        };

        Self {
            shift  : mask(xkb::MOD_NAME_SHIFT),
            lock   : mask(xkb::MOD_NAME_CAPS),
            control: mask(xkb::MOD_NAME_CTRL),
            mod1   : mask(xkb::MOD_NAME_ALT),
            mod2   : mask(xkb::MOD_NAME_NUM),
            mod3   : mask(xkb::MOD_NAME_MOD3),
            mod4   : mask(xkb::MOD_NAME_LOGO),
            mod5   : mask(xkb::MOD_NAME_ISO_LEVEL3_SHIFT),
            super_ : mask("Super"),
            hyper  : mask("Hyper"),
            meta   : mask("Meta"),
        }
    }

    /// Turns an XKB modifier mask into IBus's modifier bits.
    fn to_ibus(self, active: xkb::ModMask) -> u32 {
        let mut bits = 0;
        for (mask, bit) in [
            (self.shift, SHIFT_MASK),
            (self.lock, LOCK_MASK),
            (self.control, CONTROL_MASK),
            (self.mod1, MOD1_MASK),
            (self.mod2, MOD2_MASK),
            (self.mod3, MOD3_MASK),
            (self.mod4, MOD4_MASK),
            (self.mod5, MOD5_MASK),
            (self.super_, SUPER_MASK),
            (self.hyper, HYPER_MASK),
            (self.meta, META_MASK),
        ] {
            if mask != 0 && active & mask != 0 {
                bits |= bit;
            }
        }

        bits
    }
}

// --- Repeat ---

/// The compositor's key-repeat policy, from the grab's `repeat_info`.
///
/// Repeat is ours to implement. The compositor sends this once per grab and
/// then never synthesises a repeat itself, because a client holding a keyboard
/// grab is expected to decide for itself what repeating means. See
/// [`super::frontend`] for the timer that acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepeatInfo {
    /// Repeats per second once repeating starts. Zero disables repeat
    /// entirely, whatever the delay says.
    pub rate : i32,
    /// Milliseconds a key must be held before the first repeat.
    pub delay: i32,
}

impl Default for RepeatInfo {
    /// Repeat off. The protocol guarantees a `repeat_info` before the first
    /// key press (input-method-unstable-v2.xml:441-452), so this default only
    /// ever describes a compositor that broke that promise — in which case not
    /// repeating is the safe way to be wrong.
    fn default() -> Self {
        Self {
            rate : 0,
            delay: 0,
        }
    }
}

impl RepeatInfo {
    /// How long to wait before the first repeat, or `None` if repeat is off.
    pub fn delay(self) -> Option<std::time::Duration> {
        if self.rate <= 0 || self.delay <= 0 {
            return None;
        }

        Some(std::time::Duration::from_millis(self.delay as u64))
    }

    /// The gap between repeats, or `None` if repeat is off.
    ///
    /// `rate` is characters per *second*, so the period is its reciprocal.
    /// IBus's bridge uses the rate directly as a millisecond period
    /// (`ibuswaylandim.c`, `_process_key_event_repeat_rate_cb` scheduled with
    /// `repeat_rate`), which at the usual rate of 25 repeats every 25ms —
    /// forty times too fast.
    pub fn period(self) -> Option<std::time::Duration> {
        if self.rate <= 0 {
            return None;
        }

        Some(std::time::Duration::from_millis(1000 / self.rate as u64))
    }
}

// --- The keyboard ---

/// The XKB state the grab drives, plus the repeat policy it announced.
///
/// Lives across activations even though the grab does not: compiling a keymap
/// costs milliseconds and the compositor re-sends the same one on every new
/// grab, so keeping the last is a pure win. It is dropped only when a new
/// keymap arrives or the connection goes.
pub struct Keyboard {
    /// The xkbcommon context every keymap is compiled against.
    context: xkb::Context,
    /// The compiled keymap, kept because it answers questions the state
    /// cannot — chiefly which keys are allowed to auto-repeat.
    keymap : Option<xkb::Keymap>,
    /// The live state, absent until the first keymap arrives.
    state  : Option<xkb::State>,
    /// This keymap's modifier bit positions.
    masks  : ModMasks,
    /// The compositor's repeat policy for the current grab.
    repeat : RepeatInfo,
}

impl Keyboard {
    /// A keyboard with no keymap yet.
    pub fn new() -> Self {
        Self {
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap : None,
            state  : None,
            masks  : ModMasks::default(),
            repeat : RepeatInfo::default(),
        }
    }

    /// Whether a keymap has been compiled, and so whether any of the
    /// resolution methods can answer.
    pub fn ready(&self) -> bool {
        self.state.is_some()
    }

    /// The current repeat policy.
    pub fn repeat_info(&self) -> RepeatInfo {
        self.repeat
    }

    /// Records a new repeat policy from the grab.
    pub fn set_repeat_info(&mut self, repeat: RepeatInfo) {
        self.repeat = repeat;
    }

    /// Compiles a keymap the grab sent us and starts a fresh state on it.
    ///
    /// Re-compiled on *every* `keymap` event, unlike IBus's bridge, which
    /// keeps the first one it ever saw (`ibuswaylandim.c:1136-1139` returns
    /// early once a keymap and a state exist). A user who switches layout
    /// mid-session gets a new keymap event and the bridge goes on resolving
    /// keysyms with the old layout; we do not.
    ///
    /// The fd is read rather than mapped: it has to be forwarded to the
    /// virtual keyboard afterwards, and reading a copy out of it leaves the
    /// descriptor untouched for that. Keymaps are tens of kilobytes, so the
    /// copy is not worth avoiding.
    pub fn load_keymap(&mut self, fd: &OwnedFd, size: u32) -> Result<()> {
        use std::os::unix::fs::FileExt;

        let duplicate = fd.try_clone().context("duplicating the keymap fd")?;
        let file = std::fs::File::from(duplicate);
        let mut bytes = vec![0u8; size as usize];
        file.read_exact_at(&mut bytes, 0)
            .context("reading the keymap")?;

        // The compositor's size includes the terminating NUL, which the string
        // form of the compiler must not see.
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        let text = String::from_utf8(bytes).context("the keymap is not utf-8")?;

        let keymap = xkb::Keymap::new_from_string(
            &self.context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .context("compiling the keymap")?;

        self.masks = ModMasks::from_keymap(&keymap);
        self.state = Some(xkb::State::new(&keymap));
        self.keymap = Some(keymap);

        Ok(())
    }

    /// Forgets the keymap. Used when the connection or the grab dies, so that
    /// a stale layout can never resolve a key from the next one.
    pub fn forget(&mut self) {
        self.keymap = None;
        self.state = None;
        self.masks = ModMasks::default();
        self.repeat = RepeatInfo::default();
    }

    /// Applies a `modifiers` event from the grab.
    pub fn update_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if let Some(state) = self.state.as_mut() {
            state.update_mask(depressed, latched, locked, 0, 0, group);
        }
    }

    /// The IBus modifier bits for the state right now.
    ///
    /// Includes the locked component, which IBus's bridge leaves out
    /// (`ibuswaylandim.c:1732-1734` serialises depressed and latched only), so
    /// that Caps Lock reaches the engine as `IBUS_LOCK_MASK` the way it does
    /// from GTK. That is safe here only because the routing rules test against
    /// `IBUS_MODIFIER_FILTER`, which excludes the locks — see
    /// [`super::router`].
    pub fn ibus_modifiers(&self) -> u32 {
        let Some(state) = self.state.as_ref() else {
            return 0;
        };

        let active = state.serialize_mods(
            xkb::STATE_MODS_DEPRESSED | xkb::STATE_MODS_LATCHED | xkb::STATE_MODS_LOCKED,
        );

        self.masks.to_ibus(active)
    }

    /// Whether the layout says this key auto-repeats.
    ///
    /// The keymap knows, and asking it is the only way to get the answer
    /// right: a held Shift must not repeat, a held arrow key must, and neither
    /// of those is derivable from the keysym without a table of our own.
    /// Absent a keymap nothing repeats, which is the safe direction.
    pub fn repeats(&self, evdev: u32) -> bool {
        self.keymap
            .as_ref()
            .is_some_and(|keymap| keymap.key_repeats(xkb::Keycode::new(evdev + XKB_KEYCODE_OFFSET)))
    }

    /// The keysym an evdev keycode resolves to right now.
    pub fn keysym(&self, evdev: u32) -> xkb::Keysym {
        let Some(state) = self.state.as_ref() else {
            return xkb::Keysym::NoSymbol;
        };

        state.key_get_one_sym(xkb::Keycode::new(evdev + XKB_KEYCODE_OFFSET))
    }

    /// The character an evdev keycode produces right now, if any.
    ///
    /// `None` covers both "this key has no character" (F5) and "this key is a
    /// modifier", which is what makes the routing rule "commit plain
    /// printables" safe to state in terms of characters.
    pub fn character(&self, evdev: u32) -> Option<char> {
        let state = self.state.as_ref()?;
        let utf32 = state.key_get_utf32(xkb::Keycode::new(evdev + XKB_KEYCODE_OFFSET));
        if utf32 == 0 {
            return None;
        }

        char::from_u32(utf32)
    }

}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this is a fix for: upstream schedules the repeat timer with the
    /// rate as if it were a millisecond period. At the usual 25 characters per
    /// second that is a repeat every 25ms rather than every 40.
    #[test]
    fn the_period_is_the_reciprocal_of_the_rate() {
        let repeat = RepeatInfo { rate: 25, delay: 600 };
        assert_eq!(repeat.period(), Some(std::time::Duration::from_millis(40)));
        assert_eq!(repeat.delay(), Some(std::time::Duration::from_millis(600)));
    }

    /// "A rate of zero will disable any repeating (regardless of the value of
    /// delay)" — input-method-unstable-v2.xml:449-450.
    #[test]
    fn a_zero_rate_disables_repeat() {
        let repeat = RepeatInfo { rate: 0, delay: 600 };
        assert_eq!(repeat.period(), None);
        assert_eq!(repeat.delay(), None);
    }

    /// Negative values are illegal per the protocol, so they mean a broken
    /// compositor; not repeating is the safe reading.
    #[test]
    fn illegal_values_disable_repeat() {
        assert_eq!(RepeatInfo { rate: -1, delay: 600 }.period(), None);
        assert_eq!(RepeatInfo { rate: 25, delay: -1 }.delay(), None);
    }

    /// A keyboard with no keymap must answer, not panic: keys can arrive
    /// before the keymap event has been processed, and the frontend's
    /// behaviour then is to pass them through.
    #[test]
    fn answers_without_a_keymap() {
        let keyboard = Keyboard::new();
        assert!(!keyboard.ready());
        assert_eq!(keyboard.ibus_modifiers(), 0);
        assert_eq!(keyboard.keysym(30), xkb::Keysym::NoSymbol);
        assert_eq!(keyboard.character(30), None);
    }

    /// The whole point of the module: evdev in, meaning out, with the +8 done
    /// where nobody can forget it. Keycode 30 is `KEY_A`.
    #[test]
    fn resolves_a_key_through_a_real_keymap() {
        let mut keyboard = Keyboard::new();
        let keymap = xkb::Keymap::new_from_names(
            &keyboard.context,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile a us keymap");
        keyboard.masks = ModMasks::from_keymap(&keymap);
        keyboard.state = Some(xkb::State::new(&keymap));
        keyboard.keymap = Some(keymap);

        assert_eq!(keyboard.character(30), Some('a'));
        assert_eq!(keyboard.keysym(30).raw(), 0x0061);

        // Shift is a mask, not a key press: the frontend never calls
        // update_key, so this is exactly the path a real Shift+a takes.
        let shift = keyboard.masks.shift;
        keyboard.update_modifiers(shift, 0, 0, 0);
        assert_eq!(keyboard.character(30), Some('A'));
        assert_eq!(keyboard.ibus_modifiers(), SHIFT_MASK);

        // And Caps Lock reaches IBus as LOCK_MASK, which upstream drops.
        keyboard.update_modifiers(0, 0, keyboard.masks.lock, 0);
        assert_eq!(keyboard.ibus_modifiers(), LOCK_MASK);
    }

    /// A key that produces no text has to answer `None` rather than some
    /// placeholder character, because the routing rule keys off exactly that.
    #[test]
    fn a_function_key_has_no_character() {
        let mut keyboard = Keyboard::new();
        let keymap = xkb::Keymap::new_from_names(
            &keyboard.context,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile a us keymap");
        keyboard.masks = ModMasks::from_keymap(&keymap);
        keyboard.state = Some(xkb::State::new(&keymap));
        keyboard.keymap = Some(keymap);

        // KEY_F5 is 63, KEY_LEFTCTRL is 29.
        assert_eq!(keyboard.character(63), None);
        assert_eq!(keyboard.character(29), None);

        // And the keymap, not us, decides what repeats: a modifier does not,
        // an arrow key (KEY_LEFT, 105) does.
        assert!(!keyboard.repeats(29));
        assert!(keyboard.repeats(105));
    }
}
