//! Speech detection: utterance end, pause cut points, and whether a stretch of
//! audio holds any speech at all.
//!
//! [`SpeechDetector`] is Silero v5 via the sherpa-onnx dependency that already
//! carries the recognisers. It answers one question per window — is someone
//! talking — and the two gates turn that into decisions.
//!
//! [`SilenceGate`] serves two purposes. In press-once mode it is how an
//! utterance ends at all. In hold-to-talk mode it is a backstop: if a release
//! event is lost because the keyboard reconnected mid-utterance, this stops the
//! recording instead of letting it run until the length cap.
//!
//! [`SegmentGate`] answers a different question on the same signal: where a
//! long utterance can be cut without slicing a word. That is what lets the
//! offline pass run on completed segments while the user is still talking,
//! instead of on the whole recording after they stop. It also reports how much
//! speech each segment carried, and that is what keeps a speech-free tail away
//! from the offline model.
//!
//! Why the detector is a model and not an energy threshold: the audio between
//! the last pause and key-up is room tone, an exhale and the key's own click.
//! Decoded on its own, parakeet turns that into "Yeah.", "Mm." or "Okay." with
//! near certainty (35 of 36 synthetic tails did), while the same audio behind
//! real speech decodes to nothing. So the tail must not be decoded when it has
//! no speech in it, and a breath close to the microphone clears any RMS
//! threshold that still lets quiet speech through. Silero is trained on exactly
//! that distinction.

use anyhow::{Result, anyhow};
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};
use std::path::Path;

/// Samples per Silero inference at 16kHz. Fixed by the v5 model.
const SILERO_WINDOW: i32 = 512;

/// How many milliseconds of audio a window of samples represents.
pub fn window_ms(samples: &[f32]) -> u32 {
    (samples.len() * 1000 / crate::audio::SAMPLE_RATE as usize) as u32
}

// --- SpeechDetector ---

/// Per-window speech verdicts from Silero.
///
/// Stateful across windows, so it must be [`reset`](Self::reset) between
/// utterances. Cheap enough to run on the engine loop: one inference per 32ms
/// of audio, measured at well under a millisecond each.
pub struct SpeechDetector {
    /// The model plus sherpa-onnx's segmenting state machine around it.
    vad : VoiceActivityDetector,
}

impl SpeechDetector {
    /// Loads the Silero model at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(anyhow!("VAD model missing: {} (run `just models`)", path.display()));
        }

        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model                : Some(path.to_string_lossy().into_owned()),
                threshold            : 0.5,
                // Onset. The shortest real word worth keeping ("yes", "no") is
                // longer than this; a key click is far shorter.
                min_speech_duration  : 0.1,
                // Offset. Kept short because the gates impose their own pause
                // lengths on top; a long one here would just delay every cut.
                min_silence_duration : 0.1,
                window_size          : SILERO_WINDOW,
                // Past this sherpa-onnx force-splits a segment, which would
                // show up as a spurious pause. Nobody dictates this long.
                max_speech_duration  : 600.0,
            },
            sample_rate : crate::audio::SAMPLE_RATE as i32,
            num_threads : 1,
            ..Default::default()
        };

        let vad = VoiceActivityDetector::create(&config, 30.0)
            .ok_or_else(|| anyhow!("creating the VAD from {}", path.display()))?;

        Ok(Self { vad: vad })
    }

    /// Feeds one window. Returns whether speech is in progress at its end.
    pub fn push(&mut self, samples: &[f32]) -> bool {
        self.vad.accept_waveform(samples);
        let speaking = self.vad.detected();
        // Finished segments queue up with their audio attached; only the
        // verdict is wanted, and the ring already holds the samples.
        self.vad.clear();

        speaking
    }

    /// Forgets all state, at the start of an utterance.
    pub fn reset(&mut self) {
        self.vad.reset();
    }
}

// --- SilenceGate ---

/// Tracks trailing silence across a stream of windows.
pub struct SilenceGate {
    /// Consecutive silent milliseconds required to end an utterance.
    silence_ms : u32,
    /// Silent milliseconds accumulated so far.
    elapsed_ms : u32,
}

impl SilenceGate {
    /// Creates a gate that fires after `silence_ms` of trailing quiet.
    pub fn new(silence_ms: u32) -> Self {
        Self { silence_ms: silence_ms, elapsed_ms: 0 }
    }

    /// Feeds one window of `ms` milliseconds. Returns true when the utterance
    /// should end.
    pub fn push(&mut self, ms: u32, speech: bool) -> bool {
        if speech {
            self.elapsed_ms = 0;
        } else {
            self.elapsed_ms += ms;
        }

        self.elapsed_ms >= self.silence_ms
    }

    /// Resets accumulated silence at the start of an utterance.
    pub fn reset(&mut self) {
        self.elapsed_ms = 0;
    }
}

// --- SegmentGate ---

/// A cut point reported by [`SegmentGate::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    /// Speech the finished segment carried.
    pub voiced_ms : u32,
    /// How far before the end of the current window the cut belongs.
    ///
    /// The detector confirms speech only after hearing some of it, so the
    /// last stretch of a measured pause can already be the start of the next
    /// word. Cutting at the end of the window took the "It is" off "It is
    /// certainly"; the middle of the pause is clear of both detector lags.
    pub back_ms   : u32,
}

/// Finds cut points inside an utterance: pauses long enough to be safe.
///
/// Two conditions, and both matter. The pause must be long enough that the cut
/// lands in silence rather than between two syllables, and the segment must
/// carry enough speech to be worth a decode of its own — otherwise a hesitant
/// speaker gets carved into fragments and each one costs a model pass with the
/// fixed overhead but none of the savings.
///
/// Firing latches: [`push`](Self::push) reports a cut exactly once and then
/// starts a fresh segment, so a long pause is one boundary rather than one per
/// window. The report carries the finished segment's speech, since the caller
/// decides from it whether the segment is decoded, and where in the pause the
/// cut belongs.
pub struct SegmentGate {
    /// Consecutive silent milliseconds that make a pause a cut point. Zero
    /// disables segmentation entirely.
    pause_ms      : u32,
    /// Speech the segment must carry before a pause may end it.
    min_voiced_ms : u32,
    /// Silent milliseconds accumulated since the last voiced window.
    quiet_ms      : u32,
    /// Voiced milliseconds in the segment so far.
    voiced_ms     : u32,
}

impl SegmentGate {
    /// Creates a gate that cuts at `pause_ms` of quiet, once the segment holds
    /// at least `min_voiced_ms` of speech.
    pub fn new(pause_ms: u32, min_voiced_ms: u32) -> Self {
        Self {
            pause_ms      : pause_ms,
            min_voiced_ms : min_voiced_ms,
            quiet_ms      : 0,
            voiced_ms     : 0,
        }
    }

    /// Feeds one window of `ms` milliseconds. Reports a [`Cut`] when the
    /// segment ends in this pause.
    ///
    /// Speech is counted even with segmentation disabled, because
    /// [`voiced_ms`](Self::voiced_ms) is what decides whether the tail gets
    /// decoded at all.
    pub fn push(&mut self, ms: u32, speech: bool) -> Option<Cut> {
        if speech {
            self.quiet_ms = 0;
            self.voiced_ms += ms;
        } else {
            self.quiet_ms += ms;
        }

        if self.pause_ms == 0
            || self.quiet_ms < self.pause_ms
            || self.voiced_ms < self.min_voiced_ms
        {
            return None;
        }

        let cut = Cut { voiced_ms: self.voiced_ms, back_ms: self.quiet_ms / 2 };
        self.reset();
        Some(cut)
    }

    /// Speech heard since the last cut.
    pub fn voiced_ms(&self) -> u32 {
        self.voiced_ms
    }

    /// Starts a fresh segment, discarding what was accumulated.
    pub fn reset(&mut self) {
        self.quiet_ms = 0;
        self.voiced_ms = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_gate_fires_after_enough_quiet() {
        let mut gate = SilenceGate::new(700);
        assert!(!gate.push(60, true));
        for _ in 0..11 {
            assert!(!gate.push(60, false));
        }
        assert!(gate.push(60, false));
    }

    #[test]
    fn silence_gate_restarts_on_speech() {
        let mut gate = SilenceGate::new(120);
        assert!(!gate.push(60, false));
        assert!(!gate.push(60, true));
        assert!(!gate.push(60, false));
        assert!(gate.push(60, false));
    }

    #[test]
    fn segment_needs_speech_before_a_pause_counts() {
        let mut gate = SegmentGate::new(120, 180);
        assert_eq!(gate.push(60, true), None);
        assert_eq!(gate.push(60, false), None);
        assert_eq!(gate.push(60, false), None);
        assert_eq!(gate.push(60, true), None);
        assert_eq!(gate.push(60, true), None);
        assert_eq!(gate.push(60, false), None);
        assert_eq!(gate.push(60, false), Some(Cut { voiced_ms: 180, back_ms: 60 }));
    }

    #[test]
    fn a_cut_starts_the_next_segment_empty() {
        let mut gate = SegmentGate::new(60, 60);
        assert_eq!(gate.push(60, true), None);
        assert_eq!(gate.push(60, false), Some(Cut { voiced_ms: 60, back_ms: 30 }));
        assert_eq!(gate.voiced_ms(), 0);
        assert_eq!(gate.push(60, false), None);
        assert_eq!(gate.voiced_ms(), 0);
    }

    #[test]
    fn a_speechless_segment_can_be_cut_when_no_minimum_applies() {
        let mut gate = SegmentGate::new(60, 0);
        assert_eq!(gate.push(60, false), Some(Cut { voiced_ms: 0, back_ms: 30 }));
    }

    #[test]
    fn disabled_segmentation_still_counts_speech() {
        let mut gate = SegmentGate::new(0, 0);
        assert_eq!(gate.push(60, true), None);
        assert_eq!(gate.push(60, false), None);
        assert_eq!(gate.voiced_ms(), 60);
    }
}
