//! Speech recognition. Two models, resident, doing different jobs.
//!
//! Provisional text and final text have opposite requirements. Provisional text
//! must appear immediately and is going to be replaced anyway, so accuracy
//! barely matters. Final text is what lands in the buffer, so accuracy is all
//! that matters and a few hundred milliseconds do not. Trying to serve both from
//! one model means losing on one of them.
//!
//! So partials come from `nemotron-3.5-asr-streaming-0.6b` at 560ms chunks, a
//! cache-aware FastConformer-RNNT that decodes incrementally as audio arrives.
//! It is greedy-only in sherpa-onnx, so no hotwords, and it averages about a
//! point worse WER. Neither matters for text that exists to be overwritten.
//!
//! The commit comes from one offline pass of `parakeet-unified-en-0.6b` over the
//! whole utterance at 5.91% WER. Preedit is designed to be replaced wholesale,
//! so swapping the streaming guess for the accurate result at the end is exactly
//! the protocol's intended use rather than a workaround.
//!
//! Neither recognizer is `Send`, and inference must never run on the engine's
//! event loop, so both live on one dedicated worker thread behind channels:
//! [`spawn`] returns the pair. Commands are processed strictly in order, which
//! is what makes the Reset / Feed… / Finalize sequence race-free without any
//! shared state.
//!
//! Both models are int8 and run on the CPU. ONNX Runtime has no Vulkan execution
//! provider and its ROCm one needs a from-source build, and neither is worth
//! arranging at this size.

use anyhow::{Context, Result, anyhow};
use sherpa_onnx::{
    OfflineModelConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig,
    OnlineModelConfig, OnlineRecognizer, OnlineRecognizerConfig, OnlineStream,
    OnlineTransducerModelConfig,
};
use std::path::Path;
use std::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::Config;

/// Sample rate both models were trained at.
pub const SAMPLE_RATE: i32 = 16_000;

/// Joins a model directory with one of the fixed bundle file names.
///
/// Both bundles ship the same layout: `encoder.int8.onnx`, `decoder.int8.onnx`,
/// `joiner.int8.onnx`, `tokens.txt`.
fn bundle_file(dir: &Path, name: &str) -> Result<String> {
    let path = dir.join(name);
    if !path.exists() {
        return Err(anyhow!("model file missing: {}", path.display()));
    }

    Ok(path.to_string_lossy().into_owned())
}

// --- StreamingRecognizer ---

/// Incremental recogniser driving provisional text.
///
/// Holds decoder state across calls, so it must not be shared between
/// utterances without being reset.
pub struct StreamingRecognizer {
    /// The resident model.
    rec    : OnlineRecognizer,
    /// Decoder state for the current utterance.
    stream : OnlineStream,
    /// Last hypothesis returned, for change detection.
    last   : String,
}

impl StreamingRecognizer {
    /// Loads the streaming model.
    ///
    /// Blocking and slow. Call it off the UI thread.
    pub fn load(dir: &Path, threads: u32) -> Result<Self> {
        let config = OnlineRecognizerConfig {
            model_config: OnlineModelConfig {
                transducer: OnlineTransducerModelConfig {
                    encoder : Some(bundle_file(dir, "encoder.int8.onnx")?),
                    decoder : Some(bundle_file(dir, "decoder.int8.onnx")?),
                    joiner  : Some(bundle_file(dir, "joiner.int8.onnx")?),
                },
                tokens      : Some(bundle_file(dir, "tokens.txt")?),
                num_threads : threads as i32,
                ..Default::default()
            },
            // The nemotron export is greedy-only upstream (sherpa-onnx #3572);
            // asking for beam search here would abort inside the C library.
            decoding_method: Some("greedy_search".into()),
            ..Default::default()
        };

        let rec = OnlineRecognizer::create(&config)
            .context("creating the streaming recognizer; is the model path right?")?;
        let stream = rec.create_stream();

        Ok(Self { rec: rec, stream: stream, last: String::new() })
    }

    /// Begins a new utterance, discarding any previous decoder state.
    pub fn reset(&mut self) {
        // A fresh stream rather than `reset()`: reset keeps the encoder cache,
        // and a cache warmed on the previous utterance colours the first words
        // of the next one.
        self.stream = self.rec.create_stream();
        self.last.clear();
    }

    /// Feeds newly captured samples and returns the hypothesis if it changed.
    ///
    /// The hypothesis may revise words it previously emitted. Callers that
    /// cannot take text back must gate on stability rather than using this
    /// directly.
    pub fn feed(&mut self, samples: &[f32]) -> Option<String> {
        self.stream.accept_waveform(SAMPLE_RATE, samples);
        while self.rec.is_ready(&self.stream) {
            self.rec.decode(&self.stream);
        }

        let text = self.rec.get_result(&self.stream).map(|r| r.text)?;
        if text == self.last || text.is_empty() {
            return None;
        }

        self.last = text.clone();
        Some(text)
    }
}

// --- Recognizer ---

/// Offline recogniser producing the committed text.
pub struct Recognizer {
    /// The resident model.
    rec : OfflineRecognizer,
}

impl Recognizer {
    /// Loads the offline model.
    ///
    /// Blocking and slow. The result stays resident for the process lifetime,
    /// which is what a script cannot do and the main reason this is a daemon.
    pub fn load(dir: &Path, threads: u32, hotword_score: f32) -> Result<Self> {
        let config = OfflineRecognizerConfig {
            model_config: OfflineModelConfig {
                transducer: OfflineTransducerModelConfig {
                    encoder : Some(bundle_file(dir, "encoder.int8.onnx")?),
                    decoder : Some(bundle_file(dir, "decoder.int8.onnx")?),
                    joiner  : Some(bundle_file(dir, "joiner.int8.onnx")?),
                },
                tokens      : Some(bundle_file(dir, "tokens.txt")?),
                num_threads : threads as i32,
                model_type  : Some("nemo_transducer".into()),
                ..Default::default()
            },
            // Hotwords require beam search; greedy ignores the context graph
            // silently. Beam costs ~16% over greedy on this model, so it is on
            // unconditionally rather than toggled by whether hotwords exist.
            decoding_method : Some("modified_beam_search".into()),
            hotwords_score  : hotword_score,
            ..Default::default()
        };

        let rec = OfflineRecognizer::create(&config)
            .context("creating the offline recognizer; is the model path right?")?;

        Ok(Self { rec: rec })
    }

    /// Transcribes one complete utterance, biasing toward `hotwords`.
    ///
    /// Punctuation and casing come from the model, so the returned string needs
    /// no post-processing before injection.
    ///
    /// Blocking for roughly `0.07 × utterance seconds` at two threads.
    pub fn transcribe(&self, samples: &[f32], hotwords: &[String]) -> Result<String> {
        let stream = if hotwords.is_empty() {
            self.rec.create_stream()
        } else {
            self.rec.create_stream_with_hotwords(&hotwords.join("\n"))
        };

        stream.accept_waveform(SAMPLE_RATE, samples);
        self.rec.decode(&stream);

        let result = stream.get_result().context("offline decode produced no result")?;
        Ok(result.text.trim().to_owned())
    }
}

// --- Worker thread ---

/// Requests processed in order by the worker.
pub enum AsrCmd {
    /// Start a new utterance: clear streaming state.
    Reset,
    /// Newly captured samples for the streaming model.
    Feed(Vec<f32>),
    /// The complete utterance. Runs the offline pass and answers `Final`.
    Finalize { samples: Vec<f32>, hotwords: Vec<String> },
}

/// What the worker reports back.
pub enum AsrEvent {
    /// Both models are loaded and the pipeline is usable.
    Ready,
    /// Model loading failed; the worker has exited.
    LoadFailed(String),
    /// The streaming hypothesis changed.
    Partial(String),
    /// The offline pass finished.
    Final(Result<String, String>),
}

/// Starts the recogniser thread and returns its command and event channels.
///
/// Dropping the command sender shuts the thread down. Model loading happens on
/// the worker, so this returns immediately; wait for [`AsrEvent::Ready`].
pub fn spawn(config: &Config) -> (mpsc::Sender<AsrCmd>, UnboundedReceiver<AsrEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<AsrCmd>();
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<AsrEvent>();

    let streaming = config.streaming_model.clone();
    let offline   = config.model_path.clone();
    let threads   = config.asr_threads;
    let score     = config.hotword_score;
    let nice      = config.asr_nice;

    std::thread::Builder::new()
        .name("asr-worker".into())
        .spawn(move || {
            // On Linux setpriority with PRIO_PROCESS and pid 0 applies to the
            // calling thread, not the process. Failure just means no boost.
            // SAFETY: plain syscall wrapper, no memory involved.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };

            let loaded = StreamingRecognizer::load(&streaming, threads)
                .and_then(|s| Recognizer::load(&offline, threads, score).map(|o| (s, o)));
            let (mut stream_rec, offline_rec) = match loaded {
                Ok(pair) => pair,
                Err(e)   => {
                    let _ = evt_tx.send(AsrEvent::LoadFailed(format!("{e:#}")));
                    return;
                }
            };
            let _ = evt_tx.send(AsrEvent::Ready);

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    AsrCmd::Reset        => stream_rec.reset(),
                    AsrCmd::Feed(chunk)  => {
                        if let Some(text) = stream_rec.feed(&chunk) {
                            let _ = evt_tx.send(AsrEvent::Partial(text));
                        }
                    }
                    AsrCmd::Finalize { samples, hotwords } => {
                        let out = offline_rec
                            .transcribe(&samples, &hotwords)
                            .map_err(|e| format!("{e:#}"));
                        let _ = evt_tx.send(AsrEvent::Final(out));
                    }
                }
            }
        })
        .expect("spawning the asr worker thread");

    (cmd_tx, evt_rx)
}
