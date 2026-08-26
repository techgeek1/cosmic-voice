//! Hidden development harness: `cosmic-voice devtest <check> …`.
//!
//! Each check exercises one subsystem in isolation so the pipeline can be
//! verified stage by stage without a panel, a microphone, or a keypress.
//! Deliberately undocumented in the usage string; this is scaffolding, not
//! interface.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::time::Instant;

use crate::asr::{Recognizer, SAMPLE_RATE, StreamingRecognizer};

/// Dispatches one check by name.
pub fn run(args: &[String]) -> Result<()> {
    match args {
        [c, model, wav] if c == "decode" => decode(Path::new(model), wav),
        [c, model, wav] if c == "stream" => stream(Path::new(model), wav),
        [c, secs] if c == "capture"      => capture(secs.parse()?),
        [c] if c == "keys"               => keys(),
        [c] if c == "engine"             => engine(),
        [c] if c == "inject-probe"       => inject_probe(false),
        // DANGER: binds the seat's input-method slot. With IBus's Wayland IM
        // running this has wedged all keyboard input; recovery is
        // `ibus exit; pkill -f ibus-ui-gtk3`. Run only deliberately.
        [c] if c == "inject-probe-im"    => inject_probe(true),
        _ => Err(anyhow!(
            "checks: decode <model_dir> <wav> | stream <model_dir> <wav> | \
             capture <secs> | keys | inject-probe"
        )),
    }
}

/// Loads a 16-bit mono WAV as f32 samples, verifying the sample rate.
fn load_wav(path: &str) -> Result<Vec<f32>> {
    let wave = sherpa_onnx::Wave::read(path)
        .with_context(|| format!("reading {path}"))?;
    if wave.sample_rate() != SAMPLE_RATE {
        return Err(anyhow!("{path}: {} Hz, expected {SAMPLE_RATE}", wave.sample_rate()));
    }

    Ok(wave.samples().to_vec())
}

/// One offline pass over a WAV, timed.
fn decode(model: &Path, wav: &str) -> Result<()> {
    let samples = load_wav(wav)?;
    let dur = samples.len() as f32 / SAMPLE_RATE as f32;

    let t0 = Instant::now();
    let rec = Recognizer::load(model, 2, 1.0)?;
    println!("load    {:.2?}", t0.elapsed());

    let t0 = Instant::now();
    let text = rec.transcribe(&samples, &[])?;
    let dt = t0.elapsed();
    println!("decode  {dt:.2?}  ({dur:.2}s audio, RTF {:.4})", dt.as_secs_f32() / dur);
    println!("text    {text:?}");

    Ok(())
}

/// Streams a WAV through the online model in 560ms chunks, printing partials.
fn stream(model: &Path, wav: &str) -> Result<()> {
    let samples = load_wav(wav)?;

    let t0 = Instant::now();
    let mut rec = StreamingRecognizer::load(model, 2)?;
    println!("load    {:.2?}", t0.elapsed());

    let chunk = (SAMPLE_RATE as f32 * 0.56) as usize;
    let mut spent = std::time::Duration::ZERO;
    for (i, window) in samples.chunks(chunk).enumerate() {
        let t0 = Instant::now();
        let update = rec.feed(window);
        spent += t0.elapsed();
        if let Some(text) = update {
            println!("t={:5.2}s  {text:?}", (i + 1) as f32 * 0.56);
        }
    }
    let dur = samples.len() as f32 / SAMPLE_RATE as f32;
    println!("total   {spent:.2?} for {dur:.2}s audio, RTF {:.4}", spent.as_secs_f32() / dur);

    Ok(())
}

/// Runs the capture stream briefly and reports what arrived.
fn capture(secs: u64) -> Result<()> {
    let capture = crate::audio::Capture::start(750)?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    let from = capture.now();
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let samples = capture.read(from, capture.now());

    let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    println!(
        "captured {} samples ({:.2}s at {SAMPLE_RATE}Hz)  peak {peak:.4}  rms {rms:.4}",
        samples.len(),
        samples.len() as f32 / SAMPLE_RATE as f32,
    );

    Ok(())
}

/// Lists trigger-capable devices and reports edges for a few seconds.
fn keys() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        std::thread::spawn(move || {
            let mut watcher = crate::hotkey::Watcher::new(183);
            if let Err(e) = watcher.run_blocking(tx) {
                eprintln!("watcher: {e:#}");
            }
        });

        println!("watching for KEY_F13 edges for 10s…");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(edge)) => println!("{edge:?}"),
                Ok(None)       => break,
                Err(_)         => break,
            }
        }
        Ok(())
    })
}

/// Runs the full pipeline headless: hold the trigger and speak, text lands in
/// the focused window. The acceptance test for everything at once.
fn engine() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let handle = crate::engine::spawn(crate::config::Config::load());
        let mut events = handle.events.subscribe();
        println!("engine running; hold F13 and speak. Ctrl-C to quit.");
        loop {
            match events.recv().await {
                Ok(event) => println!("{event:?}"),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return Ok(()),
            }
        }
    })
}

/// Connects the injector and reports which paths are available. Sends nothing.
fn inject_probe(bind_im: bool) -> Result<()> {
    let mut injector = crate::inject::Injector::connect(bind_im)?;
    println!("virtual keyboard : bound");
    println!("input method     : {}", injector.im_status());
    // Give the compositor a moment to deliver activate/unavailable, then look
    // again — the interesting state arrives after the initial roundtrip.
    std::thread::sleep(std::time::Duration::from_millis(300));
    injector.pump()?;
    println!("input method     : {} (after settle)", injector.im_status());

    Ok(())
}
