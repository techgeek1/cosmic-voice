//! Local push-to-talk dictation for COSMIC.
//!
//! One binary, two roles. With no arguments it runs as a panel applet and owns
//! the whole pipeline: key watcher, audio capture, resident ASR model, and text
//! injection. With a subcommand it is a thin client that pokes the running
//! instance over a unix socket.
//!
//! The client role exists because cosmic-comp's shortcut system can only spawn
//! processes. We do not use it for the primary hotkey (see `hotkey`), but it
//! stays useful for scripting and for binding extra triggers.

mod app;
mod asr;
mod audio;
mod config;
mod devtest;
mod engine;
mod hotkey;
mod ibus;
mod im;
mod inject;
mod ipc;
mod sink;
mod toplevel;
mod transcript_log;
mod vad;

use anyhow::Result;
use ipc::Command;
use tracing_subscriber::EnvFilter;

/// Entry point. Dispatches between applet and client roles.
fn main() -> Result<()> {
    // The applet stays quiet unless RUST_LOG asks for something, because it is
    // a panel process nobody is watching. A devtest is the opposite: it exists
    // to be watched, and one that printed nothing about what it was doing to
    // the input method would be useless.
    let devtest = std::env::args().nth(1).as_deref() == Some("devtest");
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter)            => filter,
        Err(_) if devtest     => EnvFilter::new("cosmic_voice=info"),
        Err(_)                => EnvFilter::from_default_env(),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Anything past argv[0] means we are the client. Deliberately hand-rolled:
    // three verbs do not justify a dependency, and the applet path must not pay
    // for argument parsing at panel startup.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("devtest") {
        return devtest::run(&args[1..]);
    }

    match args.first().map(String::as_str) {
        None           => app::run(),
        Some("start")  => ipc::send_blocking(Command::Start),
        Some("stop")   => ipc::send_blocking(Command::Stop),
        Some("toggle") => ipc::send_blocking(Command::Toggle),
        Some("cancel") => ipc::send_blocking(Command::Cancel),
        Some("enable") => ipc::send_blocking(Command::Enable),
        Some("disable") => ipc::send_blocking(Command::Disable),
        Some("rebind") => ipc::send_blocking(Command::Rebind),
        Some("log")    => match args.get(1).map(String::as_str) {
            Some("on")  => ipc::send_blocking(Command::SetLogging(true)),
            Some("off") => ipc::send_blocking(Command::SetLogging(false)),
            _ => {
                eprintln!("usage: cosmic-voice log on|off");
                std::process::exit(2);
            }
        },
        Some(other)    => {
            eprintln!("cosmic-voice: unknown command {other:?}");
            eprintln!("usage: cosmic-voice [start|stop|toggle|cancel|enable|disable|rebind|log on|off]");
            std::process::exit(2);
        }
    }
}
