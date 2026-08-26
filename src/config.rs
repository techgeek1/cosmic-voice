//! Persisted settings.
//!
//! One RON file at `~/.config/cosmic-voice/config.ron`, written with defaults
//! on first run so there is something to edit. No settings UI yet; the file is
//! the interface.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Everything the user can tune.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Offline model producing committed text. Resident for the process life.
    pub model_path          : PathBuf,
    /// Streaming model producing provisional text. Also resident.
    pub streaming_model     : PathBuf,
    /// evdev key code to listen for. Defaults to `KEY_F13` (183).
    pub trigger_code        : u16,
    /// Hold the key to talk, versus press once to start and again to stop.
    pub hold_to_talk        : bool,
    /// Milliseconds of audio retained ahead of the trigger, so the first
    /// syllable survives the gap between speaking and the key registering.
    pub preroll_ms          : u32,
    /// Trailing silence that ends an utterance in press-once mode.
    pub silence_ms          : u32,
    /// Hard cap on a single utterance, in seconds. Also the backstop for a
    /// release event lost across a keyboard reconnect in hold mode.
    pub max_utterance_s     : u32,
    /// Whether to bind `zwp_input_method_v2` for the preedit fast path.
    ///
    /// **Off by default, deliberately.** Binding the seat's single input-method
    /// slot while IBus runs its Wayland IM (`ibus-ui-gtk3 --enable-wayland-im`)
    /// wedged cosmic-comp's keyboard routing and killed all text input
    /// session-wide — recovery required killing IBus. Enable this only after
    /// deciding who owns the slot; with it off, injection is virtual-keyboard
    /// only and no other IME is ever contended.
    pub bind_input_method   : bool,
    /// Append one space after each committed utterance, so consecutive
    /// dictations do not run together.
    pub trailing_space      : bool,
    /// Hotwords applied when no per-application override matches.
    pub default_hotwords    : Vec<String>,
    /// Per-application hotword lists, keyed by Wayland app_id.
    ///
    /// Entries must be encodable in the model's 1024-entry BPE vocabulary. The
    /// shipped bundle carries no `bpe.model`, so plain words are rejected at
    /// load with a "cannot find ID for token" error and are silently skipped.
    pub hotwords_by_app_id  : HashMap<String, Vec<String>>,
    /// Beam-search boost applied to hotwords.
    ///
    /// The usable range is far narrower than it looks and the failure mode is
    /// not graceful. Measured against this model: at 1.0 and 1.5 an unrelated
    /// hotword leaves a correct transcript untouched, at 3.0 it already corrupts
    /// punctuation, and at 6.0 decoding degenerates into the hotword's tokens
    /// repeating until the utterance ends. Treat 1.5 as a hard ceiling.
    pub hotword_score       : f32,
    /// Threads given to each recogniser.
    ///
    /// Two is the knee: measured on this machine the offline model decodes
    /// 7.4s of audio in 0.55s at two threads (1.09 core-seconds), and more
    /// threads buy latency at a worse core-seconds cost. Keep it low; the
    /// machine is busy.
    pub asr_threads         : u32,
    /// What to do for live text when preedit is unavailable.
    pub fallback_partials   : FallbackPartials,
    /// Consecutive partials a prefix must survive before it can be typed on
    /// the virtual-keyboard path in `StreamOnly` mode.
    ///
    /// Ignored under preedit, where revision is free. Raising it trades
    /// latency for a lower chance of committing a word the recogniser was
    /// about to change, which on that path cannot be undone safely.
    pub stability_frames    : u32,
    /// Nice value for the inference worker thread.
    ///
    /// Negative values need RLIMIT_NICE headroom, which membership of `@audio`
    /// already grants here. A small boost keeps dictation responsive when the
    /// machine is saturated without meaningfully slowing anything else.
    pub asr_nice            : i32,
    /// Whether transcript logging starts enabled. See `transcript_log`.
    ///
    /// The applet toggles this at runtime without writing it back here; the
    /// config value is only the state at startup.
    pub log_transcripts     : bool,
}

// --- Config ---

impl Default for Config {
    fn default() -> Self {
        Self {
            model_path          : default_model_path(),
            streaming_model     : default_streaming_path(),
            trigger_code        : 183,
            hold_to_talk        : true,
            preroll_ms          : 750,
            silence_ms          : 700,
            max_utterance_s     : 120,
            bind_input_method   : false,
            trailing_space      : true,
            default_hotwords    : Vec::new(),
            hotwords_by_app_id  : HashMap::new(),
            hotword_score       : 1.0,
            asr_threads         : 2,
            fallback_partials   : FallbackPartials::WaitForFinal,
            stability_frames    : 2,
            asr_nice            : -5,
            log_transcripts     : false,
        }
    }
}

impl Config {
    /// Loads the config file, falling back to defaults on any problem.
    ///
    /// A missing file is written out with the defaults so the user has a
    /// template to edit; a malformed one is left alone and reported, because
    /// overwriting a file the user was editing is worse than one bad start.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match ron::from_str(&text) {
                Ok(config) => config,
                Err(e)     => {
                    tracing::warn!("{}: {e}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => {
                let config = Self::default();
                if let Err(e) = config.write_template(&path) {
                    tracing::warn!("could not write default config: {e:#}");
                }
                config
            }
        }
    }

    /// Records a new trigger key in the config file.
    ///
    /// Re-reads the file rather than writing the in-memory copy, so anything
    /// the user edited since startup survives; a malformed file is left alone
    /// for the same reason `load` leaves it alone. The write goes through a
    /// rename so a crash mid-write cannot leave a truncated config behind.
    pub fn persist_trigger(code: u16) -> Result<()> {
        let path = config_path();
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => ron::from_str::<Self>(&text)
                .with_context(|| format!("{} is malformed; not overwriting it", path.display()))?,
            Err(_)   => Self::default(),
        };
        config.trigger_code = code;

        let tmp = path.with_extension("ron.tmp");
        config.write_template(&tmp)?;
        std::fs::rename(&tmp, &path).context("replacing the config file")?;

        Ok(())
    }

    /// Writes this config as a RON template.
    fn write_template(&self, path: &PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("creating the config directory")?;
        }
        let text = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .context("serialising defaults")?;
        std::fs::write(path, text).context("writing the config file")?;

        Ok(())
    }
}

/// Behaviour for live text on clients that cannot accept preedit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FallbackPartials {
    /// Type nothing until the offline model finishes, then commit once.
    ///
    /// The default. Typing a less accurate guess that cannot be corrected is
    /// worse than a short wait.
    WaitForFinal,
    /// Type the streaming model's stable prefix as it forms and treat it as
    /// final. Live text everywhere, at the cost of the offline model's
    /// accuracy: a typed word cannot be taken back.
    StreamOnly,
}

/// `~/.config/cosmic-voice/config.ron`, honouring `XDG_CONFIG_HOME`.
fn config_path() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        })
        .join("cosmic-voice/config.ron")
}

/// Returns the default model directory under `~/.local/share/cosmic-voice`.
fn default_model_path() -> PathBuf {
    data_dir().join("models/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8")
}

/// Returns the default streaming model directory.
fn default_streaming_path() -> PathBuf {
    data_dir().join("models/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8")
}

/// Where committed transcripts are appended when logging is on.
pub fn transcript_log_path() -> PathBuf {
    data_dir().join("transcripts.jsonl")
}

/// `~/.local/share/cosmic-voice`, honouring `XDG_DATA_HOME`.
fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/share")
        })
        .join("cosmic-voice")
}
