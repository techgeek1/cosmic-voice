//! Utterance end detection, and pause detection inside an utterance.
//!
//! [`SilenceGate`] serves two purposes. In press-once mode it is how an
//! utterance ends at all. In hold-to-talk mode it is a backstop: if a release
//! event is lost because the keyboard reconnected mid-utterance, this stops the
//! recording instead of letting it run until the length cap.
//!
//! [`SegmentGate`] answers a different question on the same signal: where a
//! long utterance can be cut without slicing a word. That is what lets the
//! offline pass run on completed segments while the user is still talking,
//! instead of on the whole recording after they stop.
//!
//! Silero, via the same sherpa-onnx dependency that carries the recogniser, so
//! this costs no extra crate. The energy gate below is a placeholder to get the
//! state machine moving; swap it for `sherpa_onnx::Vad` once the pipeline runs
//! end to end.

/// RMS below which a window counts as silence.
const SILENCE_RMS: f32 = 0.01;

/// Root mean square of one window, the crude voicing test both gates share.
fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// How many milliseconds of audio a window of samples represents.
fn window_ms(samples: &[f32]) -> u32 {
    (samples.len() * 1000 / crate::audio::SAMPLE_RATE as usize) as u32
}

// --- SilenceGate ---

/// Tracks trailing silence across a stream of samples.
pub struct SilenceGate {
    /// RMS below which a window counts as silence.
    threshold  : f32,
    /// Consecutive silent milliseconds required to end an utterance.
    silence_ms : u32,
    /// Silent milliseconds accumulated so far.
    elapsed_ms : u32,
}

impl SilenceGate {
    /// Creates a gate that fires after `silence_ms` of trailing quiet.
    pub fn new(silence_ms: u32) -> Self {
        Self {
            threshold  : SILENCE_RMS,
            silence_ms : silence_ms,
            elapsed_ms : 0,
        }
    }

    /// Feeds one window of samples. Returns true when the utterance should end.
    pub fn push(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return false;
        }

        if rms(samples) < self.threshold {
            self.elapsed_ms += window_ms(samples);
        } else {
            self.elapsed_ms = 0;
        }

        self.elapsed_ms >= self.silence_ms
    }

    /// Resets accumulated silence at the start of an utterance.
    pub fn reset(&mut self) {
        self.elapsed_ms = 0;
    }
}

// --- SegmentGate ---

/// Finds cut points inside an utterance: pauses long enough to be safe.
///
/// Two conditions, and both matter. The pause must be long enough that the cut
/// lands in silence rather than between two syllables, and the segment must
/// carry enough speech to be worth a decode of its own — otherwise a hesitant
/// speaker gets carved into fragments and each one costs a model pass with the
/// fixed overhead but none of the savings.
///
/// Firing latches: [`push`](Self::push) returns true exactly once per cut and
/// then starts a fresh segment, so a long pause is one boundary rather than one
/// per window.
pub struct SegmentGate {
    /// RMS below which a window counts as silence.
    threshold     : f32,
    /// Consecutive silent milliseconds that make a pause a cut point. Zero
    /// disables segmentation entirely.
    pause_ms      : u32,
    /// Speech the segment must carry before a pause may end it.
    min_voiced_ms : u32,
    /// Silent milliseconds accumulated since the last voiced window.
    quiet_ms      : u32,
    /// Non-silent milliseconds in the segment so far.
    voiced_ms     : u32,
}

impl SegmentGate {
    /// Creates a gate that cuts at `pause_ms` of quiet, once the segment holds
    /// at least `min_voiced_ms` of speech.
    pub fn new(pause_ms: u32, min_voiced_ms: u32) -> Self {
        Self {
            threshold     : SILENCE_RMS,
            pause_ms      : pause_ms,
            min_voiced_ms : min_voiced_ms,
            quiet_ms      : 0,
            voiced_ms     : 0,
        }
    }

    /// Feeds one window of samples. Returns true when the segment ends here.
    pub fn push(&mut self, samples: &[f32]) -> bool {
        if self.pause_ms == 0 || samples.is_empty() {
            return false;
        }

        let ms = window_ms(samples);
        if rms(samples) < self.threshold {
            self.quiet_ms += ms;
        } else {
            self.quiet_ms = 0;
            self.voiced_ms += ms;
        }

        if self.quiet_ms < self.pause_ms || self.voiced_ms < self.min_voiced_ms {
            return false;
        }

        self.reset();
        true
    }

    /// Starts a fresh segment, discarding what was accumulated.
    pub fn reset(&mut self) {
        self.quiet_ms = 0;
        self.voiced_ms = 0;
    }
}
