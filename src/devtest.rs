//! Hidden development harness: `cosmic-voice devtest <check> …`.
//!
//! Each check exercises one subsystem in isolation so the pipeline can be
//! verified stage by stage without a panel, a microphone, or a keypress.
//! Deliberately undocumented in the usage string; this is scaffolding, not
//! interface.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::asr::{Recognizer, SAMPLE_RATE, StreamingRecognizer};
use crate::engine::TICK_MS;
use crate::vad::SegmentGate;

/// Dispatches one check by name.
pub fn run(args: &[String]) -> Result<()> {
    match args {
        [c, model, wav] if c == "decode" => decode(Path::new(model), wav, 2),
        [c, model, wav, n] if c == "decode" => decode(Path::new(model), wav, n.parse()?),
        [c, model, wav] if c == "stream" => stream(Path::new(model), wav, 2),
        [c, model, wav, n] if c == "stream" => stream(Path::new(model), wav, n.parse()?),
        [c, model, wav] if c == "finalize" => finalize(Path::new(model), wav, 4),
        [c, model, wav, n] if c == "finalize" => finalize(Path::new(model), wav, n.parse()?),
        [c, secs] if c == "capture"      => capture(secs.parse()?),
        [c] if c == "keys"               => keys(),
        [c] if c == "rebind"             => rebind(),
        [c] if c == "engine"             => engine(),
        [c] if c == "inject-probe"       => inject_probe(false),
        [c, secs, text] if c == "type"   => type_text(secs.parse()?, text),
        [c] if c == "ibus-info"          => ibus_info(),
        // ATTENDED ONLY: takes IBus focus away from real windows for as long
        // as it runs. See the warning it prints.
        [c, engine, keys @ ..] if c == "ibus-keys" => ibus_keys(engine, keys),
        // DANGER: binds the seat's input-method slot. With IBus's Wayland IM
        // running this has wedged all keyboard input; recovery is
        // `ibus exit; pkill -f ibus-ui-gtk3`. Run only deliberately.
        [c] if c == "inject-probe-im"    => inject_probe(true),
        _ => Err(anyhow!(
            "checks: decode <model_dir> <wav> [threads] | stream <model_dir> <wav> [threads] | \
             finalize <model_dir> <wav> [threads] | capture <secs> | keys | rebind | \
             inject-probe | type <secs> <text> | ibus-info | ibus-keys <engine> <key>…"
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
fn decode(model: &Path, wav: &str, threads: u32) -> Result<()> {
    let samples = load_wav(wav)?;
    let dur = samples.len() as f32 / SAMPLE_RATE as f32;

    let t0 = Instant::now();
    let rec = Recognizer::load(model, threads, 1.0)?;
    println!("load    {:.2?}", t0.elapsed());

    let t0 = Instant::now();
    let text = rec.transcribe(&samples, &[])?;
    let dt = t0.elapsed();
    println!("decode  {dt:.2?}  ({dur:.2}s audio, RTF {:.4})", dt.as_secs_f32() / dur);
    println!("text    {text:?}");

    Ok(())
}

/// Streams a WAV through the online model in 560ms chunks, printing partials.
fn stream(model: &Path, wav: &str, threads: u32) -> Result<()> {
    let samples = load_wav(wav)?;

    let t0 = Instant::now();
    let mut rec = StreamingRecognizer::load(model, threads)?;
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

/// Replays a WAV through the engine's segmentation and offline decode, timed.
///
/// Answers the question the applet cannot be asked headlessly: how long the
/// user waits after letting go of the key. The audio is walked in the same
/// windows `engine::on_tick` uses, cut by the same [`SegmentGate`], and each
/// completed segment decoded as the engine would decode it mid-utterance. The
/// number that matters is the last line: everything before the final pause is
/// already done when the key comes up, so only the tail is latency.
///
/// `slack` is how much audio time separated a segment's cut from the next one
/// minus what its decode cost. Negative anywhere means the offline model is
/// falling behind the speaker and the backlog will surface as extra latency at
/// release.
fn finalize(model: &Path, wav: &str, threads: u32) -> Result<()> {
    let samples = load_wav(wav)?;
    let dur = samples.len() as f32 / SAMPLE_RATE as f32;
    let config = crate::config::Config::load();

    let rec = Recognizer::load(model, threads, config.hotword_score)?;
    let mut gate = SegmentGate::new(config.segment_pause_ms, config.min_segment_ms);
    let window = (SAMPLE_RATE as u64 * TICK_MS / 1000) as usize;

    println!(
        "pause {}ms  min segment {}ms  threads {threads}",
        config.segment_pause_ms, config.min_segment_ms,
    );

    let mut texts = Vec::new();
    let mut spent = std::time::Duration::ZERO;
    let mut start = 0usize;
    let mut prev_cut = 0f32;
    let mut fed = 0usize;

    for chunk in samples.chunks(window) {
        fed += chunk.len();
        if !gate.push(chunk) {
            continue;
        }

        let t0 = Instant::now();
        let text = rec.transcribe(&samples[start..fed], &config.default_hotwords)?;
        let dt = t0.elapsed();
        spent += dt;

        let at = fed as f32 / SAMPLE_RATE as f32;
        println!(
            "segment  cut at {at:6.2}s  {:6.2}s audio  decode {dt:>8.2?}  slack {:+.2}s",
            at - prev_cut,
            (at - prev_cut) - dt.as_secs_f32(),
        );
        if !text.is_empty() {
            texts.push(text);
        }
        start = fed;
        prev_cut = at;
    }

    let t0 = Instant::now();
    let text = rec.transcribe(&samples[start..], &config.default_hotwords)?;
    let tail = t0.elapsed();
    spent += tail;
    if !text.is_empty() {
        texts.push(text);
    }

    println!(
        "tail     {:6.2}s audio  decode {tail:>8.2?}   <- latency after release",
        dur - prev_cut,
    );
    println!("total    {spent:.2?} of decode over {dur:.2}s audio in {} segments", texts.len());
    println!("text     {:?}", texts.join(" "));

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
        let (mut watcher, _control) = crate::hotkey::Watcher::new(183)?;
        std::thread::spawn(move || {
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

/// Exercises the rebind path: opens every keyboard unmasked and reports the
/// first key pressed, or the timeout if none is.
fn rebind() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (mut watcher, control) = crate::hotkey::Watcher::new(183)?;
        std::thread::spawn(move || {
            if let Err(e) = watcher.run_blocking(tx) {
                eprintln!("watcher: {e:#}");
            }
        });

        control.capture();
        println!(
            "press a key within {}s (Esc cancels)…",
            crate::hotkey::CAPTURE_WINDOW.as_secs()
        );
        loop {
            match rx.recv().await {
                Some(crate::hotkey::HotkeyEvent::Rebound(Some(code))) => {
                    println!("captured {code} ({})", crate::hotkey::key_name(code));
                    break;
                }
                Some(crate::hotkey::HotkeyEvent::Rebound(None)) => {
                    println!("no key captured");
                    break;
                }
                Some(other) => println!("{other:?}"),
                None        => break,
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

/// Types `text` through the virtual keyboard after `secs` seconds, which is
/// long enough to focus the window under test. Reproduces injection faults
/// without the ASR in the loop.
fn type_text(secs: u64, text: &str) -> Result<()> {
    let mut injector = crate::inject::Injector::connect(false)?;
    println!("typing {text:?} in {secs}s — focus the target window");
    std::thread::sleep(std::time::Duration::from_secs(secs));
    injector.type_text(text)?;
    println!("done");

    Ok(())
}

/// Reads everything the ibus client can read without creating anything.
///
/// The safe half of the phase-1 acceptance test for `src/ibus`: it proves the
/// address file parses, that the private bus really is a message bus (a
/// `Hello` happened, so we have a unique name), and that the
/// `IBusSerializable` codec agrees with what the live daemon actually sends —
/// which is the part no unit test can establish, because the fixtures come
/// from the same reading of the source as the decoder.
///
/// Creates no input context and takes no focus, so it is safe to run at any
/// time against the session's real daemon.
fn ibus_info() -> Result<()> {
    let bus = crate::ibus::Bus::connect()?;
    let address = bus.address();

    println!("source        {}", address.source);
    println!("address       {}", address.address);
    println!(
        "daemon pid    {}",
        address.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
    );
    println!("unique name   {}", bus.unique_name());
    println!("daemon says   {}", bus.daemon_address()?);
    println!("ping          {:?}", bus.ping("cosmic-voice")?);
    println!("embed preedit {}", bus.embed_preedit_text()?);
    println!("focused ctx   {}", bus.current_input_context()?);

    // Write-only in the daemon despite introspecting as readable, so the
    // interesting outcome here is the error.
    match bus.global_shortcut_keys() {
        Ok((kind, keys)) => println!("shortcut keys type {kind}, {} binding(s)", keys.len()),
        Err(e)           => println!("shortcut keys unreadable: {e}"),
    }

    let global = bus.global_engine()?;
    println!("\nglobal engine\n{}", indent(&global.detail()));

    let engines = bus.engines()?;
    let active = bus.active_engines()?;
    println!("\nregistry      {} engines, {} active", engines.len(), active.len());
    for engine in &active {
        println!("  active      {engine}");
    }

    // dconf is where the user's engine order actually lives; the daemon's
    // ActiveEngines is derived from it, and phase 4 cycles through the same
    // list. Shelling out beats linking gio for one string.
    match std::process::Command::new("gsettings")
        .args(["get", "org.freedesktop.ibus.general", "preload-engines"])
        .output()
    {
        Ok(output) if output.status.success() => println!(
            "  dconf       {}",
            String::from_utf8_lossy(&output.stdout).trim(),
        ),
        Ok(output) => println!("  dconf       gsettings failed: {}", output.status),
        Err(e)     => println!("  dconf       gsettings unavailable: {e}"),
    }

    let wanted = ["mozc-jp", "xkb:us::eng"];
    println!("\nGetEnginesByNames({wanted:?})");
    for engine in bus.engines_by_names(&wanted)? {
        println!("{}", indent(&engine.detail()));
        println!();
    }

    Ok(())
}

/// Feeds a scripted key sequence to an engine through a real input context.
///
/// **Attended only.** It calls `FocusIn`, which takes the engine away from
/// whatever window the user is actually typing in, for as long as it runs.
///
/// Each key spec is either an xkbcommon keysym name (`a`, `space`, `Return`,
/// `Muhenkan`) or a single literal character, optionally suffixed with
/// `/<evdev code>` to supply a hardware keycode — IBus's convention, which
/// every engine is tested against, is evdev+8, and that offset is applied
/// here. Without a suffix the keycode is sent as 0, which engines that only
/// look at the keysym accept.
///
/// Every key is sent twice, press then release with [`crate::ibus::RELEASE_MASK`],
/// because engines that latch on release (mozc's Henkan handling among them)
/// misbehave when a client only sends presses.
fn ibus_keys(engine: &str, keys: &[String]) -> Result<()> {
    use crate::ibus::describe_capabilities;

    println!("!! WARNING: this steals IBus focus from your real windows while it runs.");
    println!("!! Keys typed elsewhere will go to this context until it exits.\n");

    let bus = crate::ibus::Bus::connect()?;
    let mut context = bus.create_input_context(crate::ibus::CLIENT_NAME)?;
    println!("context       {}", context.path());
    println!("capabilities  {}", describe_capabilities(crate::ibus::CAPABILITIES));

    // With use-global-engine on (the default) the daemon refuses per-context
    // SetEngine; the global engine is the only switch, and it moves the user's
    // real windows with it, so it is put back afterwards whatever happens.
    let previous = bus.global_engine()?.name;
    let switched = match context.set_engine(engine) {
        Ok(())  => false,
        Err(e)  => {
            println!("engine        SetEngine refused ({e}); switching the global engine");
            bus.set_global_engine(engine)?;
            true
        }
    };
    let result = ibus_keys_session(&mut context, keys);
    if switched {
        bus.set_global_engine(&previous)?;
        println!("global engine restored to {previous}");
    }

    result
}

/// The focused part of `ibus-keys`, split out so the engine switch around it
/// is undone on every exit path.
fn ibus_keys_session(context: &mut crate::ibus::Context, keys: &[String]) -> Result<()> {
    use crate::ibus::{RELEASE_MASK, Text};

    // A caret rectangle and an empty surrounding text before focus, so the
    // engine never sees the "client declared the capability but never answered"
    // state that some engines log about.
    context.set_cursor_location(0, 0, 0, 0)?;
    context.set_surrounding_text(&Text::plain(""), 0, 0)?;

    context.focus_in()?;
    // Only meaningful after focus: with use-global-engine the daemon attaches
    // the engine to whichever context is focused, and before that it is the
    // placeholder "dummy".
    println!("focus         in");
    println!("engine        {}\n", context.engine()?);

    for spec in keys {
        let (keyval, keycode) = parse_key_spec(spec)?;
        for state in [0, RELEASE_MASK] {
            let outcome = context.process_key(keyval, keycode, state)?;
            println!(
                "{spec:>12}  {}  handled={}  {} record(s)",
                if state == 0 { "press  " } else { "release" },
                outcome.handled,
                outcome.records.len(),
            );
            for record in &outcome.records {
                println!("              -> {record}");
            }
        }
        drain_signals(context, Duration::from_millis(100))?;
    }

    println!("\nwaiting 2s for asynchronous signals…");
    drain_signals(context, Duration::from_secs(2))?;

    // Reset before dropping focus so a half-finished conversion does not
    // linger in the engine for whoever gets focus next.
    context.reset()?;
    context.focus_out()?;
    println!("\nreset, focus  out");
    // Dropping the context destroys it in the daemon.

    Ok(())
}

/// Prints every signal that arrives within `window`.
fn drain_signals(context: &mut crate::ibus::Context, window: Duration) -> Result<()> {
    let deadline = Instant::now() + window;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match context.next_signal(Some(remaining))? {
            Some(signal) => println!("              ~> {signal}"),
            None         => break,
        }
    }

    Ok(())
}

/// Resolves a key spec to an X keysym and an IBus keycode.
fn parse_key_spec(spec: &str) -> Result<(u32, u32)> {
    use xkbcommon::xkb;

    let (name, evdev) = match spec.split_once('/') {
        Some((name, code)) => (name, code.parse::<u32>()?),
        None               => (spec, 0),
    };

    let mut keysym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    if keysym == xkb::Keysym::NoSymbol
        && let Some(character) = name.chars().next()
        && name.chars().count() == 1
    {
        keysym = xkb::utf32_to_keysym(character as u32);
    }
    if keysym == xkb::Keysym::NoSymbol {
        return Err(anyhow!("{spec:?}: not an xkb keysym name or a single character"));
    }

    // The GTK convention every engine is tested against, and the daemon passes
    // the value through untouched. 0 means "we do not know", which is what a
    // spec without a hardware code gets.
    let keycode = if evdev == 0 { 0 } else { evdev + 8 };

    Ok((keysym.raw(), keycode))
}

/// Indents a multi-line block for the report layout.
fn indent(block: &str) -> String {
    block
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::parse_key_spec;

    /// The keysym resolution in `ibus-keys` is the one part of that check that
    /// can be verified without a daemon and without stealing anybody's focus,
    /// so it is worth pinning: a wrong keysym looks exactly like an engine
    /// that ignored the key.
    #[test]
    fn resolves_keysym_names_and_literals() {
        assert_eq!(parse_key_spec("a").expect("a").0, 0x0061);
        assert_eq!(parse_key_spec("space").expect("space").0, 0x0020);
        assert_eq!(parse_key_spec("Return").expect("Return").0, 0xff0d);
        // Not an xkb name, so it falls through to the literal path.
        assert_eq!(parse_key_spec("あ").expect("hiragana a").0, 0x0100_3042);
    }

    /// IBus keycodes are evdev+8, the GTK convention every engine is tested
    /// against; an unsuffixed spec means "we do not know the hardware code".
    #[test]
    fn applies_the_evdev_offset() {
        assert_eq!(parse_key_spec("a/30").expect("a/30").1, 38);
        assert_eq!(parse_key_spec("a").expect("a").1, 0);
    }

    #[test]
    fn rejects_a_name_that_is_neither() {
        assert!(parse_key_spec("NotAKeysymName").is_err());
    }
}
