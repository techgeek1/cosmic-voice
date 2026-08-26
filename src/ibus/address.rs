//! Finding the socket ibus-daemon is listening on.
//!
//! ibus-daemon does not put its bus on the session bus and does not advertise
//! it through any service. It writes a file, and every client is expected to
//! read it: `~/.config/ibus/bus/<machine-id>-unix-<display>`, where the
//! machine ID is D-Bus's (`/etc/machine-id`) and the display is
//! `$WAYLAND_DISPLAY` under Wayland. The file is shell-sourceable, which is
//! the whole reason for its format:
//!
//! ```text
//! # This file is created by ibus-daemon, please do not modify it.
//! IBUS_ADDRESS=unix:path=/home/u/.cache/ibus/dbus-dSYkNYCk,guid=dbfc0029…
//! IBUS_DAEMON_PID=1985894
//! ```
//!
//! Two details make this worth a module rather than three lines inline. The
//! file outlives the daemon — nothing deletes it on exit — so the PID line is
//! load-bearing: without checking it, a client connects to a stale socket path
//! and gets a confusing `ECONNREFUSED` instead of "IBus is not running".
//! And `IBUS_ADDRESS` in the environment overrides the file entirely, which is
//! how a test harness points us at a scratch daemon (phase 2's nested
//! compositor does exactly that).

use std::path::{Path, PathBuf};

use super::{Error, Result};

/// Where a discovered address came from, kept for diagnostics: "which daemon
/// am I actually talking to" is the first question when two are running.
#[derive(Debug, Clone)]
pub enum Source {
    /// `IBUS_ADDRESS` was set in the environment and took precedence.
    Environment,
    /// Read from ibus-daemon's address file at this path.
    File(PathBuf),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Environment => write!(f, "$IBUS_ADDRESS"),
            Source::File(path)  => write!(f, "{}", path.display()),
        }
    }
}

/// A discovered ibus bus address.
#[derive(Debug, Clone)]
pub struct Address {
    /// The D-Bus address string, e.g. `unix:path=/…/dbus-dSYkNYCk,guid=…`.
    pub address: String,
    /// The daemon's PID, when the file said so. Absent for `IBUS_ADDRESS`,
    /// which carries no PID; that is not treated as an error because a test
    /// harness setting it knows what it is doing.
    pub pid    : Option<u32>,
    /// Where the address came from.
    pub source : Source,
}

impl Address {
    /// Locates the running daemon's bus.
    ///
    /// Returns [`Error::NoAddressFile`] when IBus has never run for this
    /// display and [`Error::DaemonGone`] when it has run and exited, because
    /// the caller wants to log those differently even though both mean "fall
    /// back to passing keys through".
    pub fn discover() -> Result<Self> {
        if let Ok(address) = std::env::var("IBUS_ADDRESS")
            && !address.is_empty()
        {
            return Ok(Self {
                address: address,
                pid    : None,
                source : Source::Environment,
            });
        }

        let path = Self::file_path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NoAddressFile(path));
            }
            Err(e) => {
                return Err(Error::AddressFile {
                    path  : path,
                    source: e,
                });
            }
        };

        let (address, pid) = parse(&text)
            .map_err(|reason| Error::MalformedAddress {
                path  : path.clone(),
                reason: reason,
            })?;

        // The file is not cleaned up when the daemon dies, so a readable file
        // proves nothing on its own. /proc is the cheapest liveness check
        // there is and needs no privilege.
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return Err(Error::DaemonGone {
                path: path,
                pid : pid,
            });
        }

        Ok(Self {
            address: address,
            pid    : Some(pid),
            source : Source::File(path),
        })
    }

    /// Builds the address file's path for this machine and display.
    fn file_path() -> Result<PathBuf> {
        let display = std::env::var("WAYLAND_DISPLAY").map_err(|_| Error::NoDisplay)?;
        if display.is_empty() {
            return Err(Error::NoDisplay);
        }

        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or(Error::NoDisplay)?;

        Ok(config
            .join("ibus/bus")
            .join(format!("{}-unix-{display}", machine_id()?)))
    }
}

/// Reads D-Bus's machine ID.
///
/// `/etc/machine-id` is systemd's and is what a current dbus uses;
/// `/var/lib/dbus/machine-id` is the historical location and is still a
/// symlink to the former on most distributions. ibus builds the filename from
/// whichever `dbus_get_local_machine_id()` returns, so we try both in the same
/// order.
fn machine_id() -> Result<String> {
    let read = std::fs::read_to_string("/etc/machine-id")
        .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
        .map_err(Error::MachineId)?;

    Ok(read.trim().to_string())
}

/// Splits an address file into its address and daemon PID.
///
/// Split out from [`Address::discover`] so it can be tested without a running
/// daemon. Comment lines and blanks are skipped, unknown keys ignored: the
/// file is a shell fragment and ibus has added lines to it before.
fn parse(text: &str) -> std::result::Result<(String, u32), String> {
    let mut address = None;
    let mut pid = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "IBUS_ADDRESS"    => address = Some(value.trim().to_string()),
            "IBUS_DAEMON_PID" => {
                pid = Some(
                    value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| format!("IBUS_DAEMON_PID is not a pid: {value:?}"))?,
                );
            }
            _ => {}
        }
    }

    match (address, pid) {
        (Some(address), Some(pid)) => Ok((address, pid)),
        (None, _)                  => Err("no IBUS_ADDRESS line".to_string()),
        (_, None)                  => Err("no IBUS_DAEMON_PID line".to_string()),
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::parse;

    /// The exact file this machine's ibus-daemon 1.5.34 writes, comments and
    /// all. If this stops parsing, discovery is broken for everyone.
    const REAL: &str = "\
# This file is created by ibus-daemon, please do not modify it.
# This file allows processes on the machine to find the
# ibus session bus with the below address.
# If the IBUS_ADDRESS environment variable is set, it will
# be used rather than this file.
IBUS_ADDRESS=unix:path=/home/techgeek1/.cache/ibus/dbus-dSYkNYCk,guid=dbfc0029d096de0ad527cf8d6a8e28fd
IBUS_DAEMON_PID=1985894
";

    #[test]
    fn parses_the_real_file() {
        let (address, pid) = parse(REAL).expect("the shipped format must parse");
        assert_eq!(
            address,
            "unix:path=/home/techgeek1/.cache/ibus/dbus-dSYkNYCk,\
             guid=dbfc0029d096de0ad527cf8d6a8e28fd"
        );
        assert_eq!(pid, 1985894);
    }

    /// The address value itself contains `=` signs, so the split has to be on
    /// the *first* one only. Getting this wrong truncates the socket path.
    #[test]
    fn splits_only_on_the_first_equals() {
        let (address, _) = parse("IBUS_ADDRESS=unix:path=/tmp/s,guid=ab\nIBUS_DAEMON_PID=1\n")
            .expect("parse");
        assert_eq!(address, "unix:path=/tmp/s,guid=ab");
    }

    /// Blank lines and unknown keys must not upset it: ibus has grown lines in
    /// this file across releases.
    #[test]
    fn tolerates_blanks_and_unknown_keys() {
        let text = "\n# comment\nIBUS_SOMETHING=1\n\nIBUS_ADDRESS=unix:path=/tmp/s\nIBUS_DAEMON_PID=42\n";
        assert_eq!(parse(text).expect("parse").1, 42);
    }

    #[test]
    fn rejects_a_file_without_an_address() {
        assert!(parse("IBUS_DAEMON_PID=42\n").is_err());
    }

    #[test]
    fn rejects_a_file_without_a_pid() {
        assert!(parse("IBUS_ADDRESS=unix:path=/tmp/s\n").is_err());
    }

    #[test]
    fn rejects_a_non_numeric_pid() {
        assert!(parse("IBUS_ADDRESS=unix:path=/tmp/s\nIBUS_DAEMON_PID=later\n").is_err());
    }
}
