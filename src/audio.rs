//! Microphone capture with a rolling pre-roll buffer.
//!
//! The capture stream stays open for the process lifetime rather than being
//! started on trigger. PipeWire takes somewhere between 50 and 200ms to bring a
//! stream up, and people start talking before the key registers, so an
//! on-demand stream reliably eats the first syllable. Instead the stream runs
//! continuously into a ring buffer and the trigger just marks a point in it.
//!
//! The cost is that the microphone reads as permanently in use, which shows up
//! in the panel's audio applet. That is a real trade and belongs in settings,
//! but clipped words are the worse failure.
//!
//! This is a native PipeWire client rather than a cpal one, and that choice is
//! about scheduling. The process callback below runs on PipeWire's data-loop,
//! which is SCHED_FIFO at priority 88, so it keeps getting serviced when the
//! machine is fully saturated. Going through cpal's pulse/ALSA path would put
//! the callback on an ordinary thread in this process, and an ordinary thread
//! competing with a couple of dozen test runners is exactly what drops samples.
//!
//! Nothing expensive may happen in that callback, and nothing in it may block:
//! it copies into the ring and returns. The ring is therefore lock-free — one
//! atomic store per sample plus a release on the cursor. Samples are stored as
//! `AtomicU32` bit patterns rather than plain `f32` so a reader that loses the
//! race on wraparound reads a stale value, not undefined behaviour; positions
//! are absolute `u64` sample counts so a lagging reader can detect the loss and
//! resynchronise instead of silently consuming garbage.

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Sample rate the recognisers expect. PipeWire resamples from the device's
/// native 48kHz on the way in, which is both better and cheaper than doing it
/// here.
pub const SAMPLE_RATE: u32 = 16_000;

/// Ring capacity in samples. Power of two; 2²¹ is ~131 seconds at 16kHz and
/// 8MB, comfortably above the utterance cap on a machine where memory is free.
const RING_CAPACITY: usize = 1 << 21;

// --- Ring ---

/// The buffer shared between the realtime writer and the engine's reader.
struct Ring {
    /// Sample storage, indexed by `position & (RING_CAPACITY - 1)`.
    samples : Box<[AtomicU32]>,
    /// Absolute count of samples ever written. Storing with `Release` after
    /// the samples land is what publishes them to the reader.
    head    : AtomicU64,
}

impl Ring {
    fn new() -> Arc<Self> {
        let samples = (0..RING_CAPACITY).map(|_| AtomicU32::new(0)).collect();
        Arc::new(Self { samples: samples, head: AtomicU64::new(0) })
    }

    /// Appends samples. Realtime-safe: no locks, no allocation.
    fn write(&self, chunk: &[f32]) {
        let head = self.head.load(Ordering::Relaxed);
        for (i, &s) in chunk.iter().enumerate() {
            let slot = (head as usize + i) & (RING_CAPACITY - 1);
            self.samples[slot].store(s.to_bits(), Ordering::Relaxed);
        }
        self.head.store(head + chunk.len() as u64, Ordering::Release);
    }
}

// --- Capture ---

/// A continuously running capture stream feeding the ring.
///
/// Positions are absolute sample counts since the stream started. The engine
/// owns its own cursors into that timeline; this type only answers "what is
/// the current position" and "give me the samples between two positions".
pub struct Capture {
    /// Shared with the realtime writer.
    ring       : Arc<Ring>,
    /// History to retain ahead of a trigger, in samples.
    preroll    : u64,
    /// Keeps the PipeWire thread's channel half alive.
    _thread    : std::thread::JoinHandle<()>,
}

impl Capture {
    /// Connects a capture stream and starts the graph on its own thread.
    ///
    /// Returns once the first connection attempt has been made. Format
    /// negotiation completes asynchronously, and a stream that errors — most
    /// commonly "no target node available" because a USB microphone is
    /// unplugged — is torn down and retried until one sticks, so the daemon
    /// rides out devices coming and going.
    pub fn start(preroll_ms: u32) -> Result<Self> {
        let ring = Ring::new();
        let writer = ring.clone();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("pw-capture".into())
            .spawn(move || {
                pw::init();
                let mut reported = false;
                loop {
                    let outcome = pump(writer.clone(), &ready_tx, &mut reported);
                    match outcome {
                        Ok(())  => tracing::warn!("capture stream errored; reconnecting"),
                        Err(e)  => {
                            if !reported {
                                let _ = ready_tx.send(Err(e));
                                return;
                            }
                            tracing::warn!("capture setup failed: {e:#}; retrying");
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            })
            .context("spawning the capture thread")?;

        ready_rx
            .recv()
            .context("capture thread died before reporting")??;

        Ok(Self {
            ring    : ring,
            preroll : preroll_ms as u64 * SAMPLE_RATE as u64 / 1000,
            _thread : thread,
        })
    }

    /// Absolute position of the newest captured sample.
    pub fn now(&self) -> u64 {
        self.ring.head.load(Ordering::Acquire)
    }

    /// Where an utterance triggered now should start: `now` minus the
    /// pre-roll, clamped to what actually exists.
    pub fn mark_preroll(&self) -> u64 {
        self.now().saturating_sub(self.preroll)
    }

    /// Copies the samples in `[from, to)`, clamped to the ring's live window.
    ///
    /// If the reader lagged so far that `from` has been overwritten, the lost
    /// span is skipped rather than returned as garbage; the result is then
    /// shorter than requested. With a 131-second ring that means the utterance
    /// cap was exceeded, not that the machine hiccupped.
    pub fn read(&self, from: u64, to: u64) -> Vec<f32> {
        let head = self.now();
        let to = to.min(head);
        let oldest = head.saturating_sub(RING_CAPACITY as u64);
        let from = from.max(oldest).min(to);

        let mut out = Vec::with_capacity((to - from) as usize);
        for pos in from..to {
            let bits = self.ring.samples[pos as usize & (RING_CAPACITY - 1)].load(Ordering::Relaxed);
            out.push(f32::from_bits(bits));
        }

        out
    }
}

// --- PipeWire thread ---

/// One connection attempt: owns the main loop, context and stream until the
/// stream dies. Returns `Ok` when the stream errored after connecting (retry),
/// `Err` when setup itself failed.
fn pump(
    ring: Arc<Ring>,
    ready: &std::sync::mpsc::Sender<Result<()>>,
    reported: &mut bool,
) -> Result<()> {
    {
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pipewire main loop")?;
        let context = pw::context::ContextRc::new(&mainloop, None).context("pipewire context")?;
        let core = context.connect_rc(None).context("connecting to pipewire")?;

        let mut props = properties! {
            *pw::keys::MEDIA_TYPE     => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE     => "Communication",
            *pw::keys::NODE_NAME      => "cosmic-voice",
        };
        // Escape hatch for testing and for pinning a specific microphone.
        if let Ok(target) = std::env::var("COSMIC_VOICE_SOURCE") {
            props.insert(*pw::keys::TARGET_OBJECT, target);
        }
        let stream = pw::stream::StreamBox::new(&core, "cosmic-voice-capture", props)
            .context("creating the capture stream")?;

        let listener = stream
            .add_local_listener_with_user_data(ring)
            .state_changed({
                let mainloop = mainloop.clone();
                move |_, _, old, new| {
                    tracing::debug!("capture stream: {old:?} -> {new:?}");
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        // Ends `run()` below so the outer loop reconnects.
                        mainloop.quit();
                    }
                }
            })
            .process(|stream, ring| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };

                let bytes = data.chunk().size() as usize;
                let Some(raw) = data.data() else { return };

                // Mono f32 was fixed at negotiation, so the payload is a plain
                // sample array. Chunked copy through a stack buffer keeps the
                // f32 conversion allocation-free.
                let mut window = [0f32; 256];
                for block in raw[..bytes.min(raw.len())].chunks(size_of::<f32>() * window.len()) {
                    let n = block.len() / size_of::<f32>();
                    for (i, sample) in block.chunks_exact(size_of::<f32>()).enumerate() {
                        window[i] = f32::from_le_bytes(sample.try_into().unwrap());
                    }
                    ring.write(&window[..n]);
                }
            })
            .register()
            .context("registering the stream listener")?;

        // Fix the format at the recogniser's terms: mono f32 at 16kHz.
        // PipeWire's graph does the conversion from whatever the device runs.
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::F32LE);
        info.set_rate(SAMPLE_RATE);
        info.set_channels(1);

        let pod = pw::spa::pod::Object {
            type_      : pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id         : pw::spa::param::ParamType::EnumFormat.as_raw(),
            properties : info.into(),
        };
        let bytes = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(pod),
        )
        .map_err(|e| anyhow!("serialising the format pod: {e:?}"))?
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&bytes).context("format pod")?];

        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("connecting the capture stream")?;

        if !*reported {
            *reported = true;
            let _ = ready.send(Ok(()));
        }
        mainloop.run();
        drop(listener);

        Ok(())
    }
}
