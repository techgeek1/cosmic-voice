//! What to do with a key the compositor handed us.
//!
//! This is the one part of the frontend that is a *decision* rather than
//! plumbing, so it lives on its own as a pure function over resolved facts:
//! was the key pressed or released, did the engine consume it, which modifiers
//! were down, what character does the layout give it. Nothing here touches
//! Wayland, D-Bus or the clock, which means the rules can be tested without a
//! compositor, a daemon, or a human at a keyboard — and they are, at the
//! bottom of this file. That is the whole reason for the split: the harness
//! that exercises the rest of the frontend needs a nested compositor and an
//! attended session, and these rules are where the interesting mistakes live.
//!
//! The contract is IBus's own Wayland bridge's (`ibus_wayland_im_post_key`,
//! `client/wayland/ibuswaylandim.c:1183-1245`), which every engine has been
//! tested against, with its two known bugs left out — see
//! [`super`] for the list.

use xkbcommon::xkb;

use crate::ibus::{MODIFIER_FILTER, SHIFT_MASK};

// --- Inputs ---

/// Everything the decision depends on, resolved from the grab and the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyFacts {
    /// Whether there is an IBus context behind this key at all. False while
    /// ibus-daemon is absent or restarting, and the frontend's promise for
    /// that state is that typing keeps working exactly as if no input method
    /// were installed.
    pub engine   : bool,
    /// False for a release. Releases never commit text and never repeat.
    pub pressed  : bool,
    /// Whether IBus said it consumed the key.
    pub handled  : bool,
    /// IBus modifier bits, *without* [`crate::ibus::RELEASE_MASK`].
    pub modifiers: u32,
    /// The keysym the layout resolves this key to in the current state.
    pub keysym   : xkb::Keysym,
    /// The Unicode character the layout gives this key, if it gives one.
    /// `None` for a key like F5 or Home that produces no text.
    pub character: Option<char>,
}

/// What the frontend does with the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Nothing further. Either IBus consumed the key — its effects arrived in
    /// the drain and have already been applied — or the key is one we swallow
    /// on purpose.
    Swallow,
    /// Send the character to the application as committed text rather than as
    /// a key.
    Commit(char),
    /// Replay the key through the virtual keyboard, unchanged.
    Replay,
}

// --- The rules ---

/// Decides what happens to one key.
///
/// The order of the rules is the contract, not a style choice:
///
/// 0. **No engine, no interference.** With no IBus context there is nobody to
///    have an opinion, and the frontend's job reduces to handing the key back
///    to the application unchanged. Committing plain characters as text here
///    would be *nearly* right and wrong in the ways that matter — a key event
///    an application can act on becomes text it cannot.
/// 1. **The engine's answer wins.** If IBus consumed the key, the application
///    must not also see it — that is the entire point of an input method.
/// 2. **Level latches are swallowed.** `ISO_Level{2,3,5}_Latch` are the keys
///    that select an alternate shift level. Replaying one would apply the
///    latch twice, because the compositor also tells us about it through the
///    grab's `modifiers` event, which we forward to the virtual keyboard
///    verbatim. Upstream swallows exactly these three
///    (`ibuswaylandim.c:1218-1227`).
/// 3. **A plain printable press is committed as text**, not replayed. This is
///    what makes engine-declared layouts work: an engine can say its layout is
///    `ru` while the compositor's keymap is `us`, and the application has to
///    see Cyrillic. It also means that with any IBus context active, ordinary
///    typing arrives as commits, which is what applications already expect
///    from every other IBus client. "Plain" means no modifier that upstream
///    considers shortcut-forming ([`MODIFIER_FILTER`]) except Shift — Caps
///    Lock and Num Lock are deliberately *not* in that set, or the whole path
///    would switch itself off the moment either was on.
/// 4. **Everything else is replayed** through the virtual keyboard: releases,
///    modified keys, function keys, Return, Tab, Backspace, Escape. Control
///    characters are excluded from rule 3 by value rather than by keysym
///    because a layout can put `\n` on a key that is not `Return`.
pub fn route(facts: KeyFacts) -> Route {
    if !facts.engine {
        return Route::Replay;
    }

    if facts.handled {
        return Route::Swallow;
    }

    if matches!(
        facts.keysym.raw(),
        xkb::keysyms::KEY_ISO_Level2_Latch
            | xkb::keysyms::KEY_ISO_Level3_Latch
            | xkb::keysyms::KEY_ISO_Level5_Latch
    ) {
        return Route::Swallow;
    }

    if facts.pressed
        && facts.modifiers & MODIFIER_FILTER & !SHIFT_MASK == 0
        && let Some(character) = facts.character
        && is_printable(character)
    {
        return Route::Commit(character);
    }

    Route::Replay
}

/// Whether a character is text rather than a control code.
///
/// The list is upstream's (`ibuswaylandim.c:1234-1236`) and is a list rather
/// than `char::is_control` on purpose: those six are exactly the codes that a
/// normal keyboard produces from keys applications expect as *keys* — Return,
/// Backspace, Tab, Escape, Delete — and committing them as text would, for
/// instance, insert a literal newline where the application wanted "submit".
fn is_printable(character: char) -> bool {
    !matches!(character, '\n' | '\r' | '\u{8}' | '\t' | '\u{1b}' | '\u{7f}')
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ibus::{CONTROL_MASK, LOCK_MASK, MOD2_MASK};

    /// Builds a pressed, unhandled, unmodified key carrying `character`.
    fn press(character: Option<char>) -> KeyFacts {
        KeyFacts {
            engine   : true,
            pressed  : true,
            handled  : false,
            modifiers: 0,
            keysym   : character.map_or(xkb::Keysym::NoSymbol, |c| xkb::utf32_to_keysym(c as u32)),
            character: character,
        }
    }

    /// A key the engine ate is a key the application must never see. Getting
    /// this wrong is the classic double-input bug: mozc commits 「あ」 and the
    /// terminal also gets an `a`.
    #[test]
    fn a_handled_key_goes_nowhere() {
        let facts = KeyFacts { handled: true, ..press(Some('a')) };
        assert_eq!(route(facts), Route::Swallow);
        // Even a release, and even with modifiers.
        let facts = KeyFacts {
            handled  : true,
            pressed  : false,
            modifiers: CONTROL_MASK,
            ..press(Some('a'))
        };
        assert_eq!(route(facts), Route::Swallow);
    }

    /// Rule 3, the one that makes engine-declared layouts work.
    #[test]
    fn a_plain_printable_press_commits() {
        assert_eq!(route(press(Some('a'))), Route::Commit('a'));
        assert_eq!(route(press(Some('あ'))), Route::Commit('あ'));
    }

    /// Shift is a text modifier, not a command modifier: `A` must still commit.
    #[test]
    fn shift_still_commits() {
        let facts = KeyFacts { modifiers: SHIFT_MASK, ..press(Some('A')) };
        assert_eq!(route(facts), Route::Commit('A'));
    }

    /// Ctrl-C must reach the application as a key, or every terminal breaks.
    #[test]
    fn a_control_modified_key_is_replayed() {
        let facts = KeyFacts { modifiers: CONTROL_MASK, ..press(Some('c')) };
        assert_eq!(route(facts), Route::Replay);
    }

    /// The lock modifiers are excluded from the shortcut set on purpose. If
    /// they were not, turning Caps Lock or Num Lock on would silently disable
    /// commit-as-text for as long as it stayed on — a failure that looks like
    /// "the input method randomly stopped working".
    #[test]
    fn the_locks_do_not_disable_committing() {
        let locks = LOCK_MASK | MOD2_MASK;
        let facts = KeyFacts { modifiers: locks, ..press(Some('A')) };
        assert_eq!(route(facts), Route::Commit('A'));
    }

    /// Releases are replayed, never committed — committing on both edges would
    /// type everything twice.
    #[test]
    fn a_release_is_replayed() {
        let facts = KeyFacts { pressed: false, ..press(Some('a')) };
        assert_eq!(route(facts), Route::Replay);
    }

    /// Return, Tab, Backspace, Escape and Delete are keys, not text, even
    /// though the layout gives them a character.
    #[test]
    fn control_characters_are_replayed() {
        for character in ['\n', '\r', '\u{8}', '\t', '\u{1b}', '\u{7f}'] {
            assert_eq!(route(press(Some(character))), Route::Replay, "{character:?}");
        }
    }

    /// A key with no character at all — F5, Home, a bare modifier — is a key.
    #[test]
    fn a_non_text_key_is_replayed() {
        assert_eq!(route(press(None)), Route::Replay);
    }

    /// While ibus-daemon is away every key is the application's, including the
    /// plain printables the commit rule would otherwise claim. Typing English
    /// into a terminal has to survive an IBus restart.
    #[test]
    fn everything_is_replayed_without_an_engine() {
        let facts = KeyFacts { engine: false, ..press(Some('a')) };
        assert_eq!(route(facts), Route::Replay);
    }

    /// The compositor tells us about the latch through `modifiers` as well, so
    /// replaying the key itself would apply the level shift twice.
    #[test]
    fn level_latches_are_swallowed() {
        for raw in [
            xkb::keysyms::KEY_ISO_Level2_Latch,
            xkb::keysyms::KEY_ISO_Level3_Latch,
            xkb::keysyms::KEY_ISO_Level5_Latch,
        ] {
            let facts = KeyFacts { keysym: xkb::Keysym::new(raw), ..press(None) };
            assert_eq!(route(facts), Route::Swallow);
        }
    }
}
