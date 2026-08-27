//! Translating a text field's declared *kind* from Wayland's vocabulary into
//! IBus's.
//!
//! Both protocols describe the focused field the same way — a purpose (this is
//! a password / a URL / a phone number) and a bag of hints (spell-check it,
//! capitalise sentences, it is multiline) — and both enumerate almost the same
//! members. They are not the same enumeration, which is the whole reason this
//! file exists: `terminal` is 13 in text-input-v3 and 10 in IBus, so the three
//! date/time purposes are shifted by one and a straight cast quietly turns a
//! terminal into a date field. The hints are worse: they share no bit
//! positions at all.
//!
//! The mapping is IBus's own, from its Wayland bridge
//! (`client/wayland/ibuswaylandim.c:2230-2323` in 1.5.34), so engines see
//! exactly what they saw before we replaced that bridge.
//!
//! It matters because engines act on it: mozc turns itself off for password
//! and PIN fields, and an engine that thinks a terminal is a date picker
//! behaves strangely in ways nobody will connect back to this table.

// --- text-input-v3 content hints (text-input-unstable-v3.xml:202-212) ---

/// Suggest word completions.
const HINT_COMPLETION: u32 = 0x1;
/// Suggest word corrections.
const HINT_SPELLCHECK: u32 = 0x2;
/// Switch to uppercase at the start of a sentence.
const HINT_AUTO_CAPITALIZATION: u32 = 0x4;
/// Prefer lowercase letters.
const HINT_LOWERCASE: u32 = 0x8;
/// Prefer uppercase letters.
const HINT_UPPERCASE: u32 = 0x10;
/// Prefer title casing.
const HINT_TITLECASE: u32 = 0x20;
/// Characters should be hidden.
const HINT_HIDDEN_TEXT: u32 = 0x40;
/// Typed text should not be stored.
const HINT_SENSITIVE_DATA: u32 = 0x80;
/// Only Latin characters should be entered.
const HINT_LATIN: u32 = 0x100;
/// The field is multiline.
const HINT_MULTILINE: u32 = 0x200;

// --- IBus input hints (ibustypes.h:363-377) ---

/// Suggest checking for typos.
const IBUS_HINT_SPELLCHECK: u32 = 1 << 0;
/// Suggest word completion.
const IBUS_HINT_WORD_COMPLETION: u32 = 1 << 2;
/// Suggest converting everything to lowercase.
const IBUS_HINT_LOWERCASE: u32 = 1 << 3;
/// Suggest capitalising every character.
const IBUS_HINT_UPPERCASE_CHARS: u32 = 1 << 4;
/// Suggest capitalising the first character of every word.
const IBUS_HINT_UPPERCASE_WORDS: u32 = 1 << 5;
/// Suggest capitalising the first character of every sentence.
const IBUS_HINT_UPPERCASE_SENTENCES: u32 = 1 << 6;
/// The input method should not store the text.
const IBUS_HINT_PRIVATE: u32 = 1 << 11;
/// The text is hidden, as in a password field.
const IBUS_HINT_HIDDEN_TEXT: u32 = 1 << 12;
/// Only Latin characters are wanted.
const IBUS_HINT_LATIN: u32 = 1 << 13;
/// The field is multiline.
const IBUS_HINT_MULTILINE: u32 = 1 << 14;

// --- text-input-v3 content purposes (text-input-unstable-v3.xml:223-236) ---

/// Anything goes.
const PURPOSE_NORMAL: u32 = 0;
/// Alphabetic characters only.
const PURPOSE_ALPHA: u32 = 1;
/// Digits only.
const PURPOSE_DIGITS: u32 = 2;
/// A number, including sign and decimal separator.
const PURPOSE_NUMBER: u32 = 3;
/// A phone number.
const PURPOSE_PHONE: u32 = 4;
/// A URL.
const PURPOSE_URL: u32 = 5;
/// An email address.
const PURPOSE_EMAIL: u32 = 6;
/// A person's name.
const PURPOSE_NAME: u32 = 7;
/// A password.
const PURPOSE_PASSWORD: u32 = 8;
/// A numeric password.
const PURPOSE_PIN: u32 = 9;
/// A date.
const PURPOSE_DATE: u32 = 10;
/// A time.
const PURPOSE_TIME: u32 = 11;
/// A date and a time.
const PURPOSE_DATETIME: u32 = 12;
/// A terminal, which wants control characters through untouched.
const PURPOSE_TERMINAL: u32 = 13;

// --- IBus input purposes (ibustypes.h:306-321) ---

/// Anything goes. Also the fallback for a purpose we do not recognise, which
/// is what upstream's `default:` branch amounts to.
const IBUS_PURPOSE_FREE_FORM: u32 = 0;
/// Alphabetic characters only.
const IBUS_PURPOSE_ALPHA: u32 = 1;
/// Digits only.
const IBUS_PURPOSE_DIGITS: u32 = 2;
/// A number.
const IBUS_PURPOSE_NUMBER: u32 = 3;
/// A phone number.
const IBUS_PURPOSE_PHONE: u32 = 4;
/// A URL.
const IBUS_PURPOSE_URL: u32 = 5;
/// An email address.
const IBUS_PURPOSE_EMAIL: u32 = 6;
/// A person's name.
const IBUS_PURPOSE_NAME: u32 = 7;
/// A password.
const IBUS_PURPOSE_PASSWORD: u32 = 8;
/// A numeric password.
const IBUS_PURPOSE_PIN: u32 = 9;
/// A terminal. **Ten, not thirteen** — the one place the two enumerations
/// genuinely disagree, and the reason nothing here is a cast.
const IBUS_PURPOSE_TERMINAL: u32 = 10;
/// A date.
const IBUS_PURPOSE_DATE: u32 = 11;
/// A time.
const IBUS_PURPOSE_TIME: u32 = 12;
/// A date and a time.
const IBUS_PURPOSE_DATETIME: u32 = 13;

// --- Conversion ---

/// What the frontend caches from the `content_type` event and hands to IBus.
///
/// Stored already-converted: the event is double-buffered and arrives before
/// the context may even exist, so the conversion happens once at the edge and
/// the value that gets replayed on a later `FocusIn` is the one IBus wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentType {
    /// IBus purpose, an `IBusInputPurpose`.
    pub purpose: u32,
    /// IBus hints, a bitmask of `IBusInputHints`.
    pub hints  : u32,
}

impl Default for ContentType {
    /// The state `activate` resets the field to: an ordinary free-form field
    /// with nothing special asked of it (input-method-unstable-v2.xml:169-182,
    /// which defines the defaults as text-input-v3's `normal` and `none`).
    fn default() -> Self {
        Self {
            purpose: IBUS_PURPOSE_FREE_FORM,
            hints  : 0,
        }
    }
}

impl std::fmt::Display for ContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "purpose={} hints={:#x}", purpose_name(self.purpose), self.hints)
    }
}

impl ContentType {
    /// Converts one `zwp_input_method_v2::content_type` event.
    pub fn from_wayland(hints: u32, purpose: u32) -> Self {
        Self {
            purpose: convert_purpose(purpose),
            hints  : convert_hints(hints),
        }
    }
}

/// Maps a text-input-v3 hint mask onto IBus's.
///
/// Not every hint survives: text-input-v3 has no equivalent of IBus's
/// `NO_SPELLCHECK`, `INHIBIT_OSK`, `VERTICAL_WRITING` or the emoji pair, and
/// IBus has no equivalent of the two casing hints it drops. Both directions
/// match upstream's table, deliberately, including `sensitive_data` mapping to
/// `PRIVATE` rather than to `HIDDEN_TEXT` — they are different requests
/// (do not store it, versus do not display it).
fn convert_hints(hints: u32) -> u32 {
    let mut out = 0;
    for (from, to) in [
        (HINT_COMPLETION, IBUS_HINT_WORD_COMPLETION),
        (HINT_SPELLCHECK, IBUS_HINT_SPELLCHECK),
        (HINT_AUTO_CAPITALIZATION, IBUS_HINT_UPPERCASE_SENTENCES),
        (HINT_LOWERCASE, IBUS_HINT_LOWERCASE),
        (HINT_UPPERCASE, IBUS_HINT_UPPERCASE_CHARS),
        (HINT_TITLECASE, IBUS_HINT_UPPERCASE_WORDS),
        (HINT_HIDDEN_TEXT, IBUS_HINT_HIDDEN_TEXT),
        (HINT_SENSITIVE_DATA, IBUS_HINT_PRIVATE),
        (HINT_LATIN, IBUS_HINT_LATIN),
        (HINT_MULTILINE, IBUS_HINT_MULTILINE),
    ] {
        if hints & from != 0 {
            out |= to;
        }
    }

    out
}

/// Maps a text-input-v3 purpose onto IBus's.
///
/// An unknown purpose becomes free-form rather than an error: the enumeration
/// is explicitly extensible and upstream's own comment tells engines to read
/// anything unfamiliar as free form (`ibustypes.h:299-301`).
fn convert_purpose(purpose: u32) -> u32 {
    match purpose {
        PURPOSE_NORMAL   => IBUS_PURPOSE_FREE_FORM,
        PURPOSE_ALPHA    => IBUS_PURPOSE_ALPHA,
        PURPOSE_DIGITS   => IBUS_PURPOSE_DIGITS,
        PURPOSE_NUMBER   => IBUS_PURPOSE_NUMBER,
        PURPOSE_PHONE    => IBUS_PURPOSE_PHONE,
        PURPOSE_URL      => IBUS_PURPOSE_URL,
        PURPOSE_EMAIL    => IBUS_PURPOSE_EMAIL,
        PURPOSE_NAME     => IBUS_PURPOSE_NAME,
        PURPOSE_PASSWORD => IBUS_PURPOSE_PASSWORD,
        PURPOSE_PIN      => IBUS_PURPOSE_PIN,
        PURPOSE_DATE     => IBUS_PURPOSE_DATE,
        PURPOSE_TIME     => IBUS_PURPOSE_TIME,
        PURPOSE_DATETIME => IBUS_PURPOSE_DATETIME,
        PURPOSE_TERMINAL => IBUS_PURPOSE_TERMINAL,
        unknown          => {
            tracing::debug!("unknown text-input-v3 content purpose {unknown}, using free form");
            IBUS_PURPOSE_FREE_FORM
        }
    }
}

/// Names an IBus purpose for logs.
fn purpose_name(purpose: u32) -> &'static str {
    match purpose {
        IBUS_PURPOSE_FREE_FORM => "free-form",
        IBUS_PURPOSE_ALPHA     => "alpha",
        IBUS_PURPOSE_DIGITS    => "digits",
        IBUS_PURPOSE_NUMBER    => "number",
        IBUS_PURPOSE_PHONE     => "phone",
        IBUS_PURPOSE_URL       => "url",
        IBUS_PURPOSE_EMAIL     => "email",
        IBUS_PURPOSE_NAME      => "name",
        IBUS_PURPOSE_PASSWORD  => "password",
        IBUS_PURPOSE_PIN       => "pin",
        IBUS_PURPOSE_TERMINAL  => "terminal",
        IBUS_PURPOSE_DATE      => "date",
        IBUS_PURPOSE_TIME      => "time",
        IBUS_PURPOSE_DATETIME  => "datetime",
        _                      => "?",
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The renumbering that makes this table necessary. A cast would send
    /// `terminal` through as 13, which IBus reads as `datetime`.
    #[test]
    fn renumbers_terminal_and_the_date_purposes() {
        assert_eq!(convert_purpose(PURPOSE_TERMINAL), IBUS_PURPOSE_TERMINAL);
        assert_eq!(convert_purpose(PURPOSE_TERMINAL), 10);
        assert_eq!(convert_purpose(PURPOSE_DATE), 11);
        assert_eq!(convert_purpose(PURPOSE_TIME), 12);
        assert_eq!(convert_purpose(PURPOSE_DATETIME), 13);
    }

    /// Everything below `date` happens to line up, and the test is here so
    /// that a future renumbering upstream fails loudly rather than silently.
    #[test]
    fn passes_the_purposes_that_do_line_up() {
        for purpose in [
            PURPOSE_NORMAL,
            PURPOSE_ALPHA,
            PURPOSE_DIGITS,
            PURPOSE_NUMBER,
            PURPOSE_PHONE,
            PURPOSE_URL,
            PURPOSE_EMAIL,
            PURPOSE_NAME,
            PURPOSE_PASSWORD,
            PURPOSE_PIN,
        ] {
            assert_eq!(convert_purpose(purpose), purpose);
        }
    }

    #[test]
    fn falls_back_to_free_form_for_an_unknown_purpose() {
        assert_eq!(convert_purpose(99), IBUS_PURPOSE_FREE_FORM);
    }

    /// A password field is the case that actually changes engine behaviour, so
    /// it gets its own assertion: both the purpose and the two hints a toolkit
    /// sends with it have to survive.
    #[test]
    fn converts_a_password_field() {
        let content = ContentType::from_wayland(
            HINT_HIDDEN_TEXT | HINT_SENSITIVE_DATA,
            PURPOSE_PASSWORD,
        );
        assert_eq!(content.purpose, IBUS_PURPOSE_PASSWORD);
        assert_eq!(content.hints, IBUS_HINT_HIDDEN_TEXT | IBUS_HINT_PRIVATE);
    }

    /// The hints share no bit positions at all, so a mask that would be
    /// unchanged by a cast is the clearest way to pin the translation.
    #[test]
    fn moves_every_hint_bit() {
        let content = ContentType::from_wayland(HINT_COMPLETION | HINT_SPELLCHECK, PURPOSE_NORMAL);
        assert_eq!(
            content.hints,
            IBUS_HINT_WORD_COMPLETION | IBUS_HINT_SPELLCHECK
        );
        // 0x1|0x2 == 0x3 in Wayland; 0x4|0x1 == 0x5 in IBus.
        assert_ne!(content.hints, HINT_COMPLETION | HINT_SPELLCHECK);
    }

    #[test]
    fn ignores_hints_ibus_has_no_word_for() {
        // text-input-v3 defines nothing above 0x200; a bit set there is either
        // a protocol extension or a buggy client, and must not become noise.
        assert_eq!(convert_hints(0x8000), 0);
    }

    /// The defaults `activate` resets to, which is what an engine sees for a
    /// client that never sends a content type at all.
    #[test]
    fn defaults_to_an_ordinary_field() {
        assert_eq!(
            ContentType::default(),
            ContentType { purpose: IBUS_PURPOSE_FREE_FORM, hints: 0 }
        );
    }
}
