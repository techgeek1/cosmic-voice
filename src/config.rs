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
    /// Who owns the seat's `zwp_input_method_v2` slot.
    ///
    /// **`Off` by default, deliberately.** A seat has one input-method slot
    /// and no way to share it, and binding it while somebody else holds it
    /// does not fail cleanly on cosmic-comp — it wedges keyboard input
    /// session-wide (`docs/multiplexer.md`). The mode is therefore an explicit
    /// choice, made once, after the autostart cutover.
    pub input_method        : InputMethod,
    /// The setting `input_method` replaced. Accepted for one release.
    ///
    /// `true` meant "bind the slot from the injector" — the path that caused
    /// the 2026-08 incident. It is *not* read as a request for
    /// [`InputMethod::Multiplexer`]: an old flag must never activate an
    /// exclusive seat resource on its own, so it is warned about and ignored,
    /// and the mode stays whatever `input_method` says. See
    /// [`Config::migrate`].
    ///
    /// A plain `bool` rather than an `Option<bool>` because RON spells an
    /// optional field `Some(true)` and the config files this exists to keep
    /// working spell it `true`. Never serialised, so the next config the
    /// applet writes has only the new key.
    #[serde(default, skip_serializing)]
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
    /// Threads given to the streaming recogniser.
    ///
    /// Two is the knee: it decodes far faster than real time at that width and
    /// more threads buy latency nobody is waiting on, since partials are
    /// already ahead of the speaker. Keep it low; the machine is busy.
    pub asr_threads         : u32,
    /// Threads given to the offline recogniser.
    ///
    /// Separate from `asr_threads` because the two passes are waited on
    /// differently. Nobody waits on a partial, but the offline pass over the
    /// last segment is the whole delay between releasing the key and seeing
    /// text, so it is worth spending cores on. Measured here on 60s of audio:
    /// 5.8s at two threads, 3.9s at four, no further gain at eight, and worse
    /// again at sixteen as the per-layer split stops covering its own
    /// synchronisation.
    pub offline_threads     : u32,
    /// Trailing silence that ends a segment inside a long utterance.
    ///
    /// The offline pass costs *more* than linearly in audio length — the
    /// encoder's attention is global, so at four threads 30s decodes at
    /// 0.052x real time and 120s at 0.093x — which is why a long dictation
    /// lands seconds after the key comes up. Cutting at pauses lets each
    /// completed segment decode while the user is still talking, leaving only
    /// the tail to decode on release, and keeps every pass in the cheap part
    /// of that curve. Cuts land inside silence, so no word is split; the
    /// committed text is the segments joined.
    ///
    /// Zero disables segmentation and restores the single-pass behaviour.
    pub segment_pause_ms    : u32,
    /// Speech a segment must carry before a pause may end it.
    ///
    /// Guards against carving a hesitant speaker into fragments: each segment
    /// costs one model pass with its fixed overhead, so short ones lose more
    /// than they save. Utterances shorter than this take the single-pass path
    /// unchanged.
    pub min_segment_ms      : u32,
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
    /// Engine-switch accelerators for the input-method multiplexer, in GTK
    /// syntax (`"<Control><Alt>space"`, `"<Super>space"`).
    ///
    /// Empty — the default — means read the user's own setting from dconf,
    /// `org.freedesktop.ibus.general.hotkey triggers`, which is what
    /// `ibus-ui-gtk3` reads and therefore what the desktop's input-method
    /// settings actually edit. Set it only to test a known value or to differ
    /// from IBus deliberately: whatever is here is registered with ibus-daemon
    /// as the global switch trigger, and the daemon has no unregister.
    ///
    /// Each entry also registers its Shift-modified twin as the backward
    /// cycle, unless it already contains Shift.
    pub ibus_triggers       : Vec<String>,
    /// The engine ids the switch trigger cycles through, e.g.
    /// `["xkb:us::eng", "mozc-jp"]`.
    ///
    /// Empty — the default — means read dconf `preload-engines` in the order
    /// `engines-order` remembers, which is the list the desktop's input-method
    /// settings maintain. The daemon's own `ActiveEngines` is not usable for
    /// this: it is empty unless a component has been loaded.
    pub ibus_engines        : Vec<String>,
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
            input_method        : InputMethod::Off,
            bind_input_method   : false,
            trailing_space      : true,
            default_hotwords    : Vec::new(),
            hotwords_by_app_id  : HashMap::new(),
            hotword_score       : 1.0,
            asr_threads         : 2,
            offline_threads     : 4,
            segment_pause_ms    : 500,
            min_segment_ms      : 3_000,
            fallback_partials   : FallbackPartials::WaitForFinal,
            stability_frames    : 2,
            asr_nice            : -5,
            log_transcripts     : false,
            ibus_triggers       : Vec::new(),
            ibus_engines        : Vec::new(),
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
            Ok(text) => match ron::from_str::<Self>(&text) {
                Ok(mut config) => {
                    config.migrate();
                    config
                }
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

    /// Retires old settings, once, at load.
    ///
    /// Kept as its own step rather than done in a `Deserialize` impl because
    /// it warns: a setting that quietly changes meaning between releases is
    /// how a user ends up with an input method they did not ask for, and the
    /// one this retires used to own an exclusive seat resource.
    ///
    /// Deliberately not a mapping. `bind_input_method: true` could be read as
    /// the older spelling of `input_method: Multiplexer`, and doing so would
    /// even be safer than what the flag used to do — but the multiplexer needs
    /// the autostart cutover first, and the setting that turns it on should be
    /// the one the cutover instructions name, written by the person doing it.
    /// So the flag is dropped with a warning and the mode is whatever
    /// `input_method` says.
    pub fn migrate(&mut self) {
        if !std::mem::take(&mut self.bind_input_method) {
            return;
        }

        tracing::warn!(
            "config: bind_input_method is retired and ignored; the input method stays {:?}. \
             Set `input_method: Multiplexer` after the cutover — see the README",
            self.input_method
        );
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
        // Before the write, not after: the retired key is not serialised, so a
        // rebind on a config that still spells the mode the old way would
        // erase the user's input-method choice on its way past.
        config.migrate();
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

/// Who owns the seat's input-method slot, and therefore where dictated text
/// goes when a text field has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMethod {
    /// Nothing here binds `zwp_input_method_v2`. Dictation is typed through
    /// the virtual keyboard, IBus keeps its own Wayland bridge, and the two
    /// never meet.
    ///
    /// The default, and the behaviour every release before the multiplexer
    /// shipped had.
    Off,
    /// This process owns the slot and multiplexes it: IBus's engines drive it
    /// while the user types, dictation drives it while the user talks.
    ///
    /// Requires the autostart cutover — `ibus start` instead of `ibus start
    /// --type wayland` — because two processes cannot hold the slot and the
    /// applet refuses to try. With it set but the cutover not done, the applet
    /// says so in its popup and keeps working on the virtual-keyboard path.
    Multiplexer,
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
