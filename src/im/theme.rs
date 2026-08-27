//! The candidate window's colours and metrics, read from the COSMIC theme.
//!
//! The popup is a bare `wl_surface` with pixels we produce ourselves, so
//! nothing gives it a COSMIC appearance for free the way a widget in the applet
//! gets one. This module is the substitute: it reads the same config the rest
//! of the desktop reads and hands the renderer a small, flat palette.
//!
//! # Why it can be read from this thread
//!
//! `cosmic_theme::Theme` is a `CosmicConfigEntry`, which means it is a plain
//! deserialisation of files under `~/.config/cosmic/com.system76.CosmicTheme.*`.
//! Reading one needs no iced runtime, no event loop and no GPU — which matters,
//! because the input-method thread has none of those and must not acquire any.
//! `cosmic-config` is already a direct dependency and libcosmic re-exports the
//! theme model, so this costs nothing new.
//!
//! # Read once
//!
//! At popup creation, and never again. `cosmic-config` can watch for changes,
//! but a watch is another fd on a loop whose other sources are latency-critical,
//! and a candidate window that keeps yesterday's accent colour until the next
//! login is a cosmetic problem rather than a correctness one. Live theme
//! following is future work; it is a subscription, not a redesign.

use cosmic::cosmic_theme::palette::Srgba;
use tiny_skia::Color as SkiaColor;

// --- Colours ---

/// A colour, in the only form both crates that consume it agree on.
///
/// `tiny-skia` wants unpremultiplied f32 channels and `cosmic-text` wants
/// `u8`s, so neither of their colour types can be the one this module speaks;
/// eight bits per channel is what the theme file is worth anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    /// Red, 0-255.
    pub r: u8,
    /// Green, 0-255.
    pub g: u8,
    /// Blue, 0-255.
    pub b: u8,
    /// Alpha, 0-255, straight rather than premultiplied.
    pub a: u8,
}

impl Rgba {
    /// A fully opaque colour from three channels, for the fallback table.
    const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r,
            g: g,
            b: b,
            a: 255,
        }
    }

    /// The same colour with a different alpha, for dividers and dimmed text.
    const fn with_alpha(self, a: u8) -> Self {
        Self {
            r: self.r,
            g: self.g,
            b: self.b,
            a: a,
        }
    }

    /// This colour as `tiny-skia` wants it, for fills and strokes.
    pub fn skia(self) -> SkiaColor {
        SkiaColor::from_rgba8(self.r, self.g, self.b, self.a)
    }

    /// This colour as `cosmic-text` wants it, for glyph coverage.
    pub fn text(self) -> cosmic_text::Color {
        cosmic_text::Color::rgba(self.r, self.g, self.b, self.a)
    }

    /// Converts one channel of a `palette::Srgba`, which is f32 in 0..=1.
    ///
    /// Clamped rather than trusted: the value comes from a RON file the user
    /// can edit by hand, and `as u8` on a value outside the range is a silent
    /// wrap in release builds.
    fn channel(value: f32) -> u8 {
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    }
}

impl From<Srgba> for Rgba {
    fn from(colour: Srgba) -> Self {
        Self {
            r: Rgba::channel(colour.red),
            g: Rgba::channel(colour.green),
            b: Rgba::channel(colour.blue),
            a: Rgba::channel(colour.alpha),
        }
    }
}

// --- The palette ---

/// Everything the renderer needs that is not text.
#[derive(Debug, Clone)]
pub struct Palette {
    /// The window's fill. Always opaque — see [`Palette::from_theme`].
    pub background  : Rgba,
    /// A hairline around the window, so it separates from a dark application.
    pub border      : Rgba,
    /// The rule between the auxiliary line and the candidates.
    pub divider     : Rgba,
    /// Candidate text.
    pub text        : Rgba,
    /// Labels, auxiliary text and the page counter: present but not the point.
    pub dim         : Rgba,
    /// The fill behind the focused candidate.
    pub selection   : Rgba,
    /// Text on top of that fill, which is *not* the same as [`Palette::text`]
    /// — a light accent needs dark text on it.
    pub on_selection: Rgba,
    /// Corner radius, in logical pixels.
    pub radius      : f32,
    /// Padding inside the window, in logical pixels.
    pub padding     : f32,
    /// Where this palette came from, for the log line that says which.
    pub source      : String,
}

impl Palette {
    /// Reads the palette the rest of the desktop is using.
    ///
    /// Never fails: if the theme config cannot be opened at all — no COSMIC
    /// config directory, a `cosmic-config` version mismatch — it falls back to
    /// [`Palette::fallback`] and says so in [`Palette::source`], because a
    /// candidate window with the wrong accent colour is worth having and one
    /// that refuses to appear is not.
    pub fn load() -> Self {
        use cosmic::cosmic_theme::{Theme, ThemeMode};
        use cosmic_config::CosmicConfigEntry as _;

        let dark = ThemeMode::config()
            .map_err(|e| tracing::debug!("no COSMIC theme mode config: {e}"))
            .ok()
            .and_then(|config| ThemeMode::is_dark(&config).ok())
            .unwrap_or(true);

        let config = if dark { Theme::dark_config() } else { Theme::light_config() };
        let config = match config {
            Ok(config) => config,
            Err(e)     => {
                tracing::warn!("no COSMIC theme config ({e}); using the built-in palette");
                return Self::fallback();
            }
        };

        // `get_entry` hands back a usable theme *and* the errors, because a
        // theme missing one key is still worth having with a default for it.
        // Both halves are wanted: the theme to draw with, the errors in the log
        // so a broken key is not invisible.
        let theme = match Theme::get_entry(&config) {
            Ok(theme)            => theme,
            Err((errors, theme)) => {
                tracing::warn!(
                    "COSMIC theme partially unreadable ({} keys); defaults for those",
                    errors.len(),
                );
                theme
            }
        };

        Self::from_theme(&theme, if dark { "COSMIC dark" } else { "COSMIC light" })
    }

    /// Flattens a `cosmic_theme::Theme` into the handful of values we draw.
    ///
    /// The background is forced opaque, which is a deliberate divergence from
    /// the theme. COSMIC's own popovers are translucent *and blurred*, and the
    /// blur is the part that keeps them readable; we have no compositor-side
    /// blur available on an input-popup surface, so a translucent candidate
    /// window would be kana laid over whatever text happens to be behind it.
    /// Legibility wins.
    fn from_theme(theme: &cosmic::cosmic_theme::Theme, source: &str) -> Self {
        let container = theme.background(false);
        let background = Rgba::from(container.base);
        let text = Rgba::from(container.on);

        Self {
            background  : background.with_alpha(255),
            border      : Rgba::from(container.divider),
            divider     : Rgba::from(container.divider),
            text        : text,
            dim         : text.with_alpha(150),
            selection   : Rgba::from(theme.accent.base),
            on_selection: Rgba::from(theme.accent.on),
            radius      : theme.corner_radii.radius_s[0],
            padding     : theme.spacing.space_xs as f32,
            source      : source.to_string(),
        }
    }

    /// A COSMIC-dark-looking palette for when the real one cannot be read.
    ///
    /// The values are the shipped `cosmic-dark` palette's, so the fallback
    /// looks like a COSMIC window rather than like a fallback. It is only ever
    /// reached when `cosmic-config` cannot open the theme at all.
    pub fn fallback() -> Self {
        let text = Rgba::opaque(0xE6, 0xE6, 0xE6);

        Self {
            background  : Rgba::opaque(0x1B, 0x1B, 0x1B),
            // The real theme composites its divider over the container base
            // before storing it, so these are opaque too rather than a
            // translucent white the popup's own background would show through.
            border      : Rgba::opaque(0x45, 0x45, 0x45),
            divider     : Rgba::opaque(0x3A, 0x3A, 0x3A),
            text        : text,
            dim         : text.with_alpha(150),
            selection   : Rgba::opaque(0x99, 0xC1, 0xF1),
            on_selection: Rgba::opaque(0x0A, 0x0A, 0x0A),
            radius      : 8.0,
            padding     : 8.0,
            source      : "built-in fallback".to_string(),
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The theme stores f32 channels and the renderer wants bytes; rounding
    /// the wrong way is how a "black" background comes out one step off.
    #[test]
    fn converts_channels_by_rounding() {
        let colour = Rgba::from(Srgba::new(0.0, 0.5, 1.0, 1.0));

        assert_eq!(colour.r, 0);
        assert_eq!(colour.g, 128);
        assert_eq!(colour.b, 255);
        assert_eq!(colour.a, 255);
    }

    /// A hand-edited theme file can hold anything; `as u8` on an out-of-range
    /// float wraps silently, so it is clamped first.
    #[test]
    fn clamps_channels_out_of_range() {
        let colour = Rgba::from(Srgba::new(-1.0, 2.0, 0.0, 1.0));

        assert_eq!(colour.r, 0);
        assert_eq!(colour.g, 255);
    }

    /// The fallback is what a machine with no COSMIC config draws with, so it
    /// has to be a complete, opaque palette rather than a set of zeroes.
    #[test]
    fn the_fallback_is_opaque_and_legible() {
        let palette = Palette::fallback();

        assert_eq!(palette.background.a, 255);
        assert_eq!(palette.selection.a, 255);
        assert_ne!(palette.text, palette.background);
    }
}
