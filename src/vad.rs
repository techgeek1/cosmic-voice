//! Utterance end detection.
//!
//! Serves two purposes. In press-once mode it is how an utterance ends at all.
//! In hold-to-talk mode it is a backstop: if a release event is lost because the
//! keyboard reconnected mid-utterance, this stops the recording instead of
//! letting it run until the length cap.
//!
//! Silero, via the same sherpa-onnx dependency that carries the recogniser, so
//! this costs no extra crate. The energy gate below is a placeholder to get the
//! state machine moving; swap it for `sherpa_onnx::Vad` once the pipeline runs
//! end to end.

/// Tracks trailing silence across a stream of samples.
pub struct SilenceGate {
    /// RMS below which a window counts as silence.
    threshold  : f32,
    /// Consecutive silent milliseconds required to end an utterance.
    silence_ms : u32,
    /// Silent milliseconds accumulated so far.
    elapsed_ms : u32,
}

// --- SilenceGate ---

impl SilenceGate {
    /// Creates a gate that fires after `silence_ms` of trailing quiet.
    pub fn new(silence_ms: u32) -> Self {
        Self {
            threshold  : 0.01,
            silence_ms : silence_ms,
            elapsed_ms : 0,
        }
    }

    /// Feeds one window of samples. Returns true when the utterance should end.
    pub fn push(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return false;
        }

        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        let window_ms = (samples.len() * 1000 / crate::audio::SAMPLE_RATE as usize) as u32;
        if rms < self.threshold {
            self.elapsed_ms += window_ms;
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
