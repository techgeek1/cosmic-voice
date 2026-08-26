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
//! No elevated access is required. logind tags input devices `uaccess` and puts
//! an ACL for the active session's user on them.

use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::Sender;

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

// --- Watcher ---

/// Watches every keyboard that can emit the trigger, across hotplug.
pub struct Watcher {
    /// evdev code being watched, e.g. 183 for `KEY_F13`.
    trigger : u16,
}

impl Watcher {
    /// Creates a watcher for the given evdev key code.
    pub fn new(trigger: u16) -> Self {
        Self { trigger: trigger }
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
    pub fn run_blocking(&mut self, tx: Sender<KeyEdge>) -> Result<()> {
        let monitor = udev::MonitorBuilder::new()
            .and_then(|m| m.match_subsystem("input"))
            .and_then(|m| m.listen())
            .context("creating the udev monitor")?;

        let mut held = false;

        'rebuild: loop {
            let mut devices: Vec<OwnedFd> = Vec::new();
            for path in self.discover()? {
                match open_filtered(&path, self.trigger) {
                    Ok(fd) => {
                        tracing::info!("watching {} for the trigger", path.display());
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
                let mut polls: Vec<PollFd> = Vec::with_capacity(devices.len() + 1);
                polls.push(PollFd::new(&monitor, PollFlags::IN));
                for fd in &devices {
                    polls.push(PollFd::new(fd, PollFlags::IN));
                }
                rustix::event::poll(&mut polls, None).context("poll")?;

                let monitor_ready = polls[0].revents().contains(PollFlags::IN);
                let ready: Vec<usize> = polls[1..]
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| !p.revents().is_empty())
                    .map(|(i, _)| i)
                    .collect();
                drop(polls);

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
                            Ok(events) => {
                                for ev in &events {
                                    let Some(edge) = decode(ev, self.trigger) else { continue };
                                    let down = edge == KeyEdge::Pressed;
                                    if held != down {
                                        held = down;
                                        if tx.blocking_send(edge).is_err() {
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
    /// Finds every `/dev/input/event*` whose key bitmap advertises the trigger.
    ///
    /// QMK boards expose several HID interfaces and the trigger may arrive on
    /// any of them, so this matches on capability rather than on device name.
    fn discover(&self) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/dev/input").context("listing /dev/input")? {
            let path = entry.context("reading /dev/input")?.path();
            let is_event = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"));
            if is_event && advertises(&path, self.trigger) {
                found.push(path);
            }
        }
        found.sort();

        Ok(found)
    }
}

/// Whether the device at `path` can emit `trigger` at all.
///
/// Returns false for devices we cannot open; those are also devices we could
/// never read, so they are simply not candidates.
fn advertises(path: &Path, trigger: u16) -> bool {
    use rustix::fs::{Mode, OFlags, open};

    let Ok(fd) = open(path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty()) else {
        return false;
    };

    let mut bits = [0u8; KEY_BITMAP_LEN];
    // SAFETY: `bits` lives across the call and is exactly the length the ioctl
    // number encodes; the descriptor is a live evdev node.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGBIT_KEY, bits.as_mut_ptr()) };

    rc >= 0 && bits[trigger as usize / 8] & (1 << (trigger % 8)) != 0
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
