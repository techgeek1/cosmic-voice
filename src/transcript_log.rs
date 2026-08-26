//! Raw transcript capture, for building a post-processing corpus.
//!
//! Every committed utterance is appended as one JSON line, exactly as the
//! offline model produced it and before anything downstream touches it. The
//! point is a dataset of what dictation actually sounds like coming out of
//! the recogniser, so a cleanup pass can be evaluated against real input
//! rather than guessed at. Off by default; togglable from the applet so a
//! session that would pollute the corpus can be kept out of it.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One committed utterance.
#[derive(Debug, Serialize)]
pub struct Record<'a> {
    /// Unix time at commit, in seconds.
    pub ts       : f64,
    /// Length of the audio handed to the offline model, pre-roll included.
    pub audio_ms : u64,
    /// The streaming model's last hypothesis before finalisation. Kept so the
    /// two models can be compared on the same audio.
    pub partial  : &'a str,
    /// The offline model's transcript: what was injected, before any
    /// trailing space.
    pub text     : &'a str,
}

/// Appends one record to the log.
///
/// Opens, appends, and closes per call: the log grows by one line per
/// utterance and holding the file open would only complicate rotating it
/// from outside while the applet runs.
pub fn append(path: &PathBuf, record: &Record<'_>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating the log directory")?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;

    let mut line = serde_json::to_vec(record).context("encoding the record")?;
    line.push(b'\n');
    file.write_all(&line).context("appending the record")?;

    Ok(())
}

/// Seconds since the Unix epoch, for [`Record::ts`].
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
