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
//! One exception, decided in the engine (`engine::choose_final`): the offline
//! model is English-only and the streaming one is multilingual, so Japanese
//! speech comes back from the offline pass as nothing, or as English sounds.
//! Then the streaming hypothesis is the commit. Measured before deciding to
//! keep two models: on a 30s English recording the streaming final reads
//! "keywacher … each own Fred on their own" where the offline pass is
//! word-perfect, so the streaming model is not a candidate for the English
//! commit.
//!
//! Neither recognizer is `Send`, and inference must never run on the engine's
//! event loop, so each lives on a dedicated worker thread behind channels:
//! [`spawn`] returns both plus the shared event stream. Each thread processes
//! its own commands strictly in order, which is what makes the Reset / Feed…
//! sequence race-free without any shared state.
//!
//! One thread each rather than one thread for both, because the offline pass
//! no longer waits for the end of the utterance. The engine cuts a long
//! utterance at pauses and hands each completed segment over while the user is
//! still speaking, so a segment decode has to overlap the partials rather than
//! stall behind them — and, more to the point, the tail decode that the user
//! actually waits on must never queue behind a backlog of streaming chunks.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
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
    /// Blocking for roughly `0.10 × utterance seconds` at two threads and
    /// `0.06 ×` at four.
    pub fn transcribe(&self, samples: &[f32], hotwords: &[String]) -> Result<String> {
        // A segment can come out empty when a cut and the key release land in
        // the same window. Nothing to decode, and the C library has no reason
        // to be asked.
        if samples.is_empty() {
            return Ok(String::new());
        }

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

// --- Worker threads ---

/// Requests processed in order by the streaming worker.
pub enum StreamCmd {
    /// Start a new utterance: clear streaming state.
    Reset,
    /// Newly captured samples for the streaming model.
    Feed(Vec<f32>),
}

/// Requests processed in order by the offline worker.
pub enum OfflineCmd {
    /// One segment of an utterance. Answers `Final` carrying the same `seq`.
    ///
    /// `seq` identifies the utterance, not the segment: the engine bumps it
    /// when an utterance is cancelled, which is how results for work it no
    /// longer wants are recognised and dropped.
    Transcribe { seq: u64, samples: Vec<f32>, hotwords: Vec<String> },
}

/// What the workers report back.
pub enum AsrEvent {
    /// Both models are loaded and the pipeline is usable.
    Ready,
    /// Model loading failed; the worker has exited.
    LoadFailed(String),
    /// The streaming hypothesis changed.
    Partial(String),
    /// One segment's offline pass finished.
    Final { seq: u64, text: Result<String, String> },
}

/// The engine's connection to the two recogniser threads.
pub struct Asr {
    /// Commands into the streaming recogniser.
    pub stream  : mpsc::Sender<StreamCmd>,
    /// Commands into the offline recogniser.
    pub offline : mpsc::Sender<OfflineCmd>,
    /// Results out of both.
    pub events  : UnboundedReceiver<AsrEvent>,
}

/// Starts both recogniser threads and returns their channels.
///
/// Dropping a command sender shuts its thread down. Model loading happens on
/// the workers, so this returns immediately; wait for [`AsrEvent::Ready`],
/// which is sent once *both* models are up.
pub fn spawn(config: &Config) -> Asr {
    let (stream_tx, stream_rx) = mpsc::channel::<StreamCmd>();
    let (offline_tx, offline_rx) = mpsc::channel::<OfflineCmd>();
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<AsrEvent>();

    // Ready is one event for two loads, so whichever finishes second sends it.
    let pending = Arc::new(AtomicU32::new(2));

    let streaming_dir = config.streaming_model.clone();
    let offline_dir   = config.model_path.clone();
    let stream_thr    = config.asr_threads;
    let offline_thr   = config.offline_threads;
    let score         = config.hotword_score;
    let nice          = config.asr_nice;

    let tx = evt_tx.clone();
    let ready = pending.clone();
    std::thread::Builder::new()
        .name("asr-stream".into())
        .spawn(move || {
            renice(nice);

            let mut rec = match StreamingRecognizer::load(&streaming_dir, stream_thr) {
                Ok(rec) => rec,
                Err(e)  => {
                    let _ = tx.send(AsrEvent::LoadFailed(format!("{e:#}")));
                    return;
                }
            };
            announce_ready(&ready, &tx);

            while let Ok(cmd) = stream_rx.recv() {
                match cmd {
                    StreamCmd::Reset       => rec.reset(),
                    StreamCmd::Feed(chunk) => {
                        if let Some(text) = rec.feed(&chunk) {
                            let _ = tx.send(AsrEvent::Partial(text));
                        }
                    }
                }
            }
        })
        .expect("spawning the streaming asr thread");

    std::thread::Builder::new()
        .name("asr-offline".into())
        .spawn(move || {
            renice(nice);

            let rec = match Recognizer::load(&offline_dir, offline_thr, score) {
                Ok(rec) => rec,
                Err(e)  => {
                    let _ = evt_tx.send(AsrEvent::LoadFailed(format!("{e:#}")));
                    return;
                }
            };
            announce_ready(&pending, &evt_tx);

            while let Ok(cmd) = offline_rx.recv() {
                match cmd {
                    OfflineCmd::Transcribe { seq, samples, hotwords } => {
                        let out = rec
                            .transcribe(&samples, &hotwords)
                            .map_err(|e| format!("{e:#}"));
                        let _ = evt_tx.send(AsrEvent::Final { seq: seq, text: out });
                    }
                }
            }
        })
        .expect("spawning the offline asr thread");

    Asr { stream: stream_tx, offline: offline_tx, events: evt_rx }
}

/// Applies the configured nice value to the calling thread.
///
/// On Linux setpriority with PRIO_PROCESS and pid 0 applies to the calling
/// thread, not the process. Failure just means no boost.
fn renice(nice: i32) {
    // SAFETY: plain syscall wrapper, no memory involved.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
}

/// Sends [`AsrEvent::Ready`] once every model has finished loading.
fn announce_ready(pending: &AtomicU32, events: &tokio::sync::mpsc::UnboundedSender<AsrEvent>) {
    if pending.fetch_sub(1, Ordering::AcqRel) == 1 {
        let _ = events.send(AsrEvent::Ready);
    }
}
