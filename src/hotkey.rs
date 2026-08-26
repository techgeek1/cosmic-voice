//! Trigger key watching, straight off evdev.
//!
//! cosmic-comp's shortcut config can only spawn a process on key *press*, and
//! `xdg-desktop-portal-cosmic` does not implement the GlobalShortcuts
//! interface, so neither can give us the release event a hold-to-talk trigger
//! needs. We read the device instead.
//!
//! Reading evdev normally means seeing every keystroke on the keyboard. We do
//! not: `EVIOCSMASK` installs a per-descriptor, kernel-enforced delivery filter,
//! so the only key event that ever reaches this process is the trigger. That is
//! a filter on delivery, not a privilege boundary. The descriptor still holds
//! read access and this process could widen the mask again. What it buys is that
//! no other keycode is ever copied into our address space, which keeps them out
//! of memory, core dumps and logs. For a boundary the kernel would enforce
//! against us, the trigger has to leave the keyboard on a different endpoint
//! entirely, which means a QMK raw-HID report on the vendor interface.
//!
//! The one deliberate exception is rebinding. To learn which key the user
//! wants, the watcher briefly opens every keyboard with no key mask at all and
//! takes the first press; that is a user-initiated window of at most
//! [`CAPTURE_WINDOW`], after which the narrow mask goes back on. See
//! [`Watcher::run_blocking`].
//!
//! No elevated access is required. logind tags input devices `uaccess` and puts
//! an ACL for the active session's user on them.

use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

/// How long a rebind waits for a key before giving up.
pub const CAPTURE_WINDOW: Duration = Duration::from_secs(10);

/// `KEY_ESC`, which cancels a rebind rather than becoming the trigger.
const KEY_ESC: u16 = 1;

/// First code of the `BTN_*` range. Anything at or above it is a button, not a
/// key, and is never offered as a trigger.
const BTN_MISC: u16 = 0x100;

/// evdev event type for key state changes.
const EV_KEY: u16 = 0x01;

/// evdev event type carrying `MSC_SCAN`, the raw scancode of every keypress.
///
/// Masking `EV_KEY` alone is not enough. `MSC_SCAN` reports the hardware
/// scancode for every key on the device and would leak the entire keyboard
/// through a side channel, so this type is masked to empty.
const EV_MSC: u16 = 0x04;

/// `_IOW('E', 0x93, struct input_mask)`, where `input_mask` is 16 bytes.
const EVIOCSMASK: u64 = (1 << 30) | (16 << 16) | ((b'E' as u64) << 8) | 0x93;

/// Bytes in a key-capability bitmap: `(KEY_MAX + 1) / 8` with `KEY_MAX` 0x2ff.
const KEY_BITMAP_LEN: usize = 96;

/// `EVIOCGBIT(EV_KEY, KEY_BITMAP_LEN)`: read a device's key capabilities.
const EVIOCGBIT_KEY: u64 =
    (2 << 30) | ((KEY_BITMAP_LEN as u64) << 16) | ((b'E' as u64) << 8) | (0x20 + EV_KEY as u64);

/// Kernel's `struct input_mask`.
#[repr(C)]
struct InputMask {
    /// Event type the mask applies to.
    type_      : u32,
    /// Length of the bitmap in bytes. Must be a multiple of `size_of::<c_long>()`.
    codes_size : u32,
    /// Userspace pointer to the bitmap.
    codes_ptr  : u64,
}

/// Kernel's `struct input_event`, 24 bytes on 64-bit.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct InputEvent {
    /// `timeval::tv_sec`.
    sec   : i64,
    /// `timeval::tv_usec`.
    usec  : i64,
    /// Event type, one of `EV_*`.
    type_ : u16,
    /// Key code within the type.
    code  : u16,
    /// For `EV_KEY`: 0 release, 1 press, 2 autorepeat.
    value : i32,
}

/// What the watcher reports upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEdge {
    /// Trigger went down.
    Pressed,
    /// Trigger came up.
    Released,
}

/// Everything the watcher sends to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// The trigger changed state.
    Edge(KeyEdge),
    /// A rebind finished. `Some` carries the new trigger, already in effect;
    /// `None` means the window expired or the user pressed Escape, and the
    /// old trigger is back in effect.
    Rebound(Option<u16>),
}

/// Requests into the watcher thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// Open every keyboard unmasked and take the next key press as the trigger.
    Capture,
}

/// The engine's handle on the watcher thread.
///
/// The thread blocks in `poll(2)`, so a request has two halves: the payload
/// through a mutex, and a write to an eventfd that sits in the poll set to
/// wake the thread up and make it look.
#[derive(Clone)]
pub struct Control {
    /// Pending requests, drained by the thread when the eventfd fires.
    pending : Arc<Mutex<Vec<Request>>>,
    /// Wakeup for the poll loop.
    wake    : Arc<OwnedFd>,
}

impl Control {
    /// Asks the watcher to rebind. The outcome arrives as
    /// [`HotkeyEvent::Rebound`] on the event channel.
    pub fn capture(&self) {
        self.pending.lock().unwrap().push(Request::Capture);
        // A full counter is the only failure, and it means the thread is
        // already due to wake; the request is queued either way.
        let _ = rustix::io::write(&self.wake, &1u64.to_ne_bytes());
    }
}

/// What the poll loop is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Watching the trigger through the narrow mask.
    Watching,
    /// Unmasked, waiting for any key press until the deadline.
    Capturing { until: Instant },
}

// --- Watcher ---

/// Watches every keyboard that can emit the trigger, across hotplug.
pub struct Watcher {
    /// evdev code being watched, e.g. 183 for `KEY_F13`.
    trigger : u16,
    /// What the loop is currently doing.
    mode    : Mode,
    /// Requests from the engine.
    control : Control,
}

impl Watcher {
    /// Creates a watcher for the given evdev key code, and the handle the
    /// engine drives it with.
    pub fn new(trigger: u16) -> Result<(Self, Control)> {
        let wake = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )
        .context("creating the watcher eventfd")?;
        let control = Control { pending: Arc::default(), wake: Arc::new(wake) };
        let watcher = Self { trigger: trigger, mode: Mode::Watching, control: control.clone() };

        Ok((watcher, control))
    }

    /// Runs until the receiver goes away, sending every press and release.
    ///
    /// Blocking: this owns its thread, polling the device descriptors and the
    /// udev monitor together. udev's socket handle is not `Send`, so a plain
    /// thread with `poll(2)` is simpler than fighting an async runtime for a
    /// handful of descriptors. Edges cross into the engine's runtime through
    /// `blocking_send`.
    ///
    /// The keyboard is wireless, so its event node number changes across
    /// reconnects and a path resolved once at startup goes stale; any add or
    /// remove in the input subsystem rebuilds the descriptor set. The same
    /// physical key can also be advertised by several endpoints of one
    /// keyboard, so only *transitions* of a shared held-state are reported,
    /// whichever descriptor delivers them first.
    ///
    /// A rebind request switches the loop into capture mode: every keyboard is
    /// reopened with no key mask, and the first key press (Escape excepted)
    /// becomes the trigger. The window closes on that press or after
    /// [`CAPTURE_WINDOW`], and either way the descriptors are rebuilt with the
    /// narrow mask before anything else is read.
    pub fn run_blocking(&mut self, tx: Sender<HotkeyEvent>) -> Result<()> {
        let monitor = udev::MonitorBuilder::new()
            .and_then(|m| m.match_subsystem("input"))
            .and_then(|m| m.listen())
            .context("creating the udev monitor")?;

        let mut held = false;

        'rebuild: loop {
            let capturing = matches!(self.mode, Mode::Capturing { .. });
            let mut devices: Vec<OwnedFd> = Vec::new();
            for path in self.discover(capturing)? {
                let opened = if capturing {
                    open_unmasked(&path)
                } else {
                    open_filtered(&path, self.trigger)
                };
                match opened {
                    Ok(fd) => {
                        tracing::info!("watching {}", path.display());
                        drain(&fd);
                        devices.push(fd);
                    }
                    // Expected for capable devices without a uaccess ACL, like
                    // a gaming mouse's consumer interface.
                    Err(e) => tracing::debug!("skipping {}: {e:#}", path.display()),
                }
            }
            if devices.is_empty() {
                tracing::warn!("no accessible device advertises the trigger; waiting for hotplug");
            }

            loop {
                let mut polls: Vec<PollFd> = Vec::with_capacity(devices.len() + 2);
                polls.push(PollFd::new(&monitor, PollFlags::IN));
                polls.push(PollFd::new(&self.control.wake, PollFlags::IN));
                for fd in &devices {
                    polls.push(PollFd::new(fd, PollFlags::IN));
                }
                let timeout = match self.mode {
                    Mode::Watching            => None,
                    Mode::Capturing { until } => {
                        let left = until.saturating_duration_since(Instant::now());
                        Some(rustix::event::Timespec {
                            tv_sec  : left.as_secs() as _,
                            tv_nsec : left.subsec_nanos() as _,
                        })
                    }
                };
                rustix::event::poll(&mut polls, timeout.as_ref()).context("poll")?;

                let monitor_ready = polls[0].revents().contains(PollFlags::IN);
                let wake_ready    = polls[1].revents().contains(PollFlags::IN);
                let ready: Vec<usize> = polls[2..]
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| !p.revents().is_empty())
                    .map(|(i, _)| i)
                    .collect();
                drop(polls);

                if wake_ready {
                    let mut buf = [0u8; 8];
                    let _ = rustix::io::read(&self.control.wake, &mut buf);
                    let requests = std::mem::take(&mut *self.control.pending.lock().unwrap());
                    if requests.contains(&Request::Capture) && !capturing {
                        tracing::info!("rebinding: waiting for a key press");
                        self.mode = Mode::Capturing { until: Instant::now() + CAPTURE_WINDOW };
                        held = false;
                        continue 'rebuild;
                    }
                }

                if let Mode::Capturing { until } = self.mode
                    && Instant::now() >= until
                {
                    tracing::info!("rebinding: timed out");
                    self.mode = Mode::Watching;
                    if tx.blocking_send(HotkeyEvent::Rebound(None)).is_err() {
                        return Ok(());
                    }
                    continue 'rebuild;
                }

                if monitor_ready {
                    let changed = monitor.iter().any(|ev| {
                        matches!(ev.event_type(), udev::EventType::Add | udev::EventType::Remove)
                    });
                    if changed {
                        // Give logind a moment to put uaccess ACLs on new nodes.
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        continue 'rebuild;
                    }
                }

                for index in ready {
                    loop {
                        match read_events(&devices[index]) {
                            Ok(events) if capturing => {
                                let Some(code) = events.iter().find_map(pressed_key) else {
                                    continue;
                                };
                                let chosen = (code != KEY_ESC).then_some(code);
                                match chosen {
                                    Some(code) => {
                                        tracing::info!("rebinding: trigger is now {code}");
                                        self.trigger = code;
                                    }
                                    None => tracing::info!("rebinding: cancelled"),
                                }
                                self.mode = Mode::Watching;
                                if tx.blocking_send(HotkeyEvent::Rebound(chosen)).is_err() {
                                    return Ok(());
                                }
                                continue 'rebuild;
                            }
                            Ok(events) => {
                                for ev in &events {
                                    let Some(edge) = decode(ev, self.trigger) else { continue };
                                    let down = edge == KeyEdge::Pressed;
                                    if held != down {
                                        held = down;
                                        if tx.blocking_send(HotkeyEvent::Edge(edge)).is_err() {
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                            Err(Errno::AGAIN) => break,
                            // Device unplugged; udev will also say so, but do
                            // not wait for it with a dead fd in the poll set.
                            Err(_) => continue 'rebuild,
                        }
                    }
                }
            }
        }
    }
}

impl Watcher {
    /// Finds every `/dev/input/event*` whose key bitmap advertises the trigger,
    /// or, when `any_key`, any key at all.
    ///
    /// QMK boards expose several HID interfaces and the trigger may arrive on
    /// any of them, so this matches on capability rather than on device name.
    fn discover(&self, any_key: bool) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/dev/input").context("listing /dev/input")? {
            let path = entry.context("reading /dev/input")?.path();
            let is_event = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"));
            if !is_event {
                continue;
            }
            let Some(bits) = key_capabilities(&path) else { continue };
            let wanted = if any_key {
                // Only the KEY_ range counts; a mouse advertising BTN_ codes
                // alone is not a keyboard.
                bits[..BTN_MISC as usize / 8].iter().any(|b| *b != 0)
            } else {
                bits[self.trigger as usize / 8] & (1 << (self.trigger % 8)) != 0
            };
            if wanted {
                found.push(path);
            }
        }
        found.sort();

        Ok(found)
    }
}

/// Reads the key-capability bitmap of the device at `path`.
///
/// Returns `None` for devices we cannot open; those are also devices we could
/// never read, so they are simply not candidates.
fn key_capabilities(path: &Path) -> Option<[u8; KEY_BITMAP_LEN]> {
    use rustix::fs::{Mode, OFlags, open};

    let fd = open(path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty()).ok()?;

    let mut bits = [0u8; KEY_BITMAP_LEN];
    // SAFETY: `bits` lives across the call and is exactly the length the ioctl
    // number encodes; the descriptor is a live evdev node.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGBIT_KEY, bits.as_mut_ptr()) };

    (rc >= 0).then_some(bits)
}

/// Reads and parses whatever complete events are currently available.
fn read_events(fd: &OwnedFd) -> std::result::Result<Vec<InputEvent>, Errno> {
    let mut buf = [0u8; size_of::<InputEvent>() * 32];
    let n = rustix::io::read(fd, &mut buf)?;

    let mut events = Vec::new();
    for chunk in buf[..n].chunks_exact(size_of::<InputEvent>()) {
        events.push(InputEvent {
            sec   : 0,
            usec  : 0,
            type_ : u16::from_ne_bytes([chunk[16], chunk[17]]),
            code  : u16::from_ne_bytes([chunk[18], chunk[19]]),
            value : i32::from_ne_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]),
        });
    }

    Ok(events)
}

/// Discards everything currently readable on a descriptor.
fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 4096];
    while matches!(rustix::io::read(fd, &mut buf), Ok(n) if n > 0) {}
}

// --- Filtered open ---

/// Opens an evdev node and installs the delivery filter before any read.
///
/// The mask is per descriptor and applies from the moment it is set, but the
/// kernel starts queueing events for a client at `open()`. Anything that landed
/// in the gap is still delivered, so callers must discard whatever is already
/// readable before entering their loop.
pub fn open_filtered(path: &Path, trigger: u16) -> Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, open};

    let fd = open(path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())
        .with_context(|| format!("opening {}", path.display()))?;

    // Restrict EV_KEY delivery to the trigger alone. `bits_from_user()` rejects
    // a length that is not a multiple of sizeof(long) with EINVAL, so the
    // bitmap is rounded up rather than sized to the exact byte holding the bit.
    let need  = (trigger as usize / 8) + 1;
    let align = size_of::<libc::c_long>();
    let len   = need.div_ceil(align) * align;

    let mut bits = vec![0u8; len];
    bits[trigger as usize / 8] = 1 << (trigger % 8);

    let key_mask = InputMask {
        type_      : EV_KEY as u32,
        codes_size : len as u32,
        codes_ptr  : bits.as_ptr() as u64,
    };
    ioctl_set_mask(&fd, &key_mask).context("restricting EV_KEY delivery")?;

    // Drop EV_MSC entirely. A zero-length bitmap clears the type, which is what
    // stops MSC_SCAN from reporting scancodes for keys we are not watching.
    let msc_mask = InputMask {
        type_      : EV_MSC as u32,
        codes_size : 0,
        codes_ptr  : 0,
    };
    ioctl_set_mask(&fd, &msc_mask).context("suppressing MSC_SCAN scancodes")?;

    Ok(fd)
}

/// Opens an evdev node for rebinding: every key is delivered, scancodes are not.
///
/// Only ever used inside a capture window, and every descriptor opened this
/// way is closed when the window ends. `MSC_SCAN` stays suppressed because the
/// key code is all a rebind needs.
fn open_unmasked(path: &Path) -> Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, open};

    let fd = open(path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())
        .with_context(|| format!("opening {}", path.display()))?;

    let msc_mask = InputMask {
        type_      : EV_MSC as u32,
        codes_size : 0,
        codes_ptr  : 0,
    };
    ioctl_set_mask(&fd, &msc_mask).context("suppressing MSC_SCAN scancodes")?;

    Ok(fd)
}

/// Issues `EVIOCSMASK` on a descriptor.
fn ioctl_set_mask(fd: &OwnedFd, mask: &InputMask) -> Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: `mask` outlives the call, `codes_ptr` points at a `codes_size`
    // byte allocation owned by the caller, and the descriptor is a live evdev
    // node opened by `open_filtered`.
    let rc = unsafe {
        libc::ioctl(fd.as_raw_fd(), EVIOCSMASK, mask as *const InputMask)
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("EVIOCSMASK");
    }

    Ok(())
}

/// The key code of a press event in the `KEY_` range, for rebinding.
fn pressed_key(ev: &InputEvent) -> Option<u16> {
    (ev.type_ == EV_KEY && ev.value == 1 && ev.code < BTN_MISC).then_some(ev.code)
}

/// Decodes one `input_event` into an edge, ignoring autorepeat.
///
/// Autorepeat arrives as `value == 2` while the key is held. Treating it as a
/// press would restart capture mid-utterance.
fn decode(ev: &InputEvent, trigger: u16) -> Option<KeyEdge> {
    if ev.type_ != EV_KEY || ev.code != trigger {
        return None;
    }

    match ev.value {
        0 => Some(KeyEdge::Released),
        1 => Some(KeyEdge::Pressed),
        _ => None,
    }
}

/// A human-readable name for an evdev key code, for the applet.
///
/// Covers the keys anyone would plausibly bind; everything else is shown by
/// number, which is also what the config file takes.
pub fn key_name(code: u16) -> String {
    let name = match code {
        1        => "Esc",
        29       => "Left Ctrl",
        41       => "`",
        42       => "Left Shift",
        54       => "Right Shift",
        56       => "Left Alt",
        58       => "Caps Lock",
        59..=68  => return format!("F{}", code - 58),
        69       => "Num Lock",
        70       => "Scroll Lock",
        87       => "F11",
        88       => "F12",
        97       => "Right Ctrl",
        99       => "SysRq",
        100      => "Right Alt",
        102      => "Home",
        104      => "Page Up",
        107      => "End",
        109      => "Page Down",
        110      => "Insert",
        111      => "Delete",
        119      => "Pause",
        125      => "Left Meta",
        126      => "Right Meta",
        127      => "Menu",
        183..=194 => return format!("F{}", code - 170),
        _        => return format!("key {code}"),
    };

    name.to_owned()
}
