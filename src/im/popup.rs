//! The candidate window: a `zwp_input_popup_surface_v2` and the state behind it.
//!
//! This is the half of phase 3 that touches Wayland. [`super::render`] turns a
//! [`View`] into pixels; this decides what the view is, when to redraw, and how
//! the pixels reach the compositor.
//!
//! # Lifecycle: one surface, for the life of the process
//!
//! The popup surface is created once, at bind time, and lives as long as the
//! process — *not* per activation alongside the grab and the virtual keyboard. The
//! protocol makes the compositor responsible for visibility ("visible if and
//! only if the input method is in the active state",
//! `input-method-unstable-v2.xml:366-375`), so there is nothing an
//! activation-scoped object would buy. Two details of smithay's implementation
//! decide it:
//!
//! - `activate_input_method` re-registers whatever popup exists against the
//!   newly focused text input on *every* activation — dismiss, `set_parent`,
//!   `new_popup` (`wayland/input_method/input_method_handle.rs:125-143`). A
//!   long-lived surface is therefore adopted by each new field for free.
//! - `text_input_rectangle` is only delivered to a popup that already exists
//!   when the text input updates its cursor rectangle (`:103-121`). A surface
//!   created inside `activate` would miss the rectangle that arrived with the
//!   activation and sit at a stale position until the caret next moved.
//!
//! There is one ordering hazard and it is covered: creating the popup before
//! any text input has focus means `get_parent()` is `None` and smithay skips
//! `new_popup` (`:272-274`), so the surface starts untracked. The first
//! `activate` fixes that, because it re-registers unconditionally.
//!
//! Hiding is a null-buffer commit. There is no request for it — the interface
//! has exactly one event and one destructor — so the only lever is core
//! Wayland: a `wl_surface` with no buffer is unmapped, and cosmic-comp derives
//! the popup's extent from `bbox_from_surface_tree`
//! (`xdg_shell/popup.rs:181`), which is empty for an unmapped surface.
//!
//! # Redraw
//!
//! Coalesced twice. The lookup-table signals arrive in bursts — one keystroke
//! through mozc produces `UpdateAuxiliaryText` and `UpdateLookupTable`, and a
//! commit produces `HideLookupTable` then `HideAuxiliaryText` — each as its own
//! callback on the signal channel, so a paint per signal would attach a buffer
//! for a state nobody should see. State changes therefore only set a dirty flag
//! and [`Popup::flush`] draws once per event-loop pass, after the whole burst
//! has been applied. On top of that, `wl_surface.frame` throttles to one buffer
//! per frame.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use tiny_skia::Pixmap;
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_v2::ZwpInputMethodV2,
    zwp_input_popup_surface_v2::{self, ZwpInputPopupSurfaceV2},
};

use super::frontend::Frontend;
use super::render::{self, Renderer, Row, View};
use super::theme::Palette;
use crate::ibus::{
    ContextSignal, LookupTable, ORIENTATION_HORIZONTAL, ORIENTATION_SYSTEM, ORIENTATION_VERTICAL,
    Text,
};

/// Labels IBus falls back to when the engine supplies none.
///
/// Verbatim from `ui/gtk3/candidatearea.vala:38-41`, including the `"0."` in
/// tenth place and the hex-looking tail, because a user who has learned that
/// the tenth candidate is `0` on IBus's panel should not have to relearn it
/// here. Note these are indexed by *slot within the page*, not by candidate.
const DEFAULT_LABELS: [&str; 16] = [
    "1.", "2.", "3.", "4.", "5.", "6.", "7.", "8.",
    "9.", "0.", "a.", "b.", "c.", "d.", "e.", "f.",
];

/// Bytes reserved per buffer slot.
///
/// [`super::render`] clamps the window to 720×560, so this is that at four
/// bytes a pixel with room to spare. Fixed rather than grown on demand: the
/// candidate window changes size on nearly every keystroke, and re-creating a
/// pool for each one would mean a `memfd_create` per keypress.
const SLOT_BYTES: usize = 720 * 576 * 4;

/// How many buffers rotate.
///
/// Two is enough because the frame callback already throttles us to one buffer
/// per frame; the second exists so a redraw does not have to wait for the
/// compositor to finish with the first.
const SLOTS: usize = 2;

// --- State ---

/// A buffer in the pool, and whether the compositor is still reading it.
struct Slot {
    /// Byte offset into the pool.
    offset: usize,
    /// The protocol object, once one has been created at the current size.
    buffer: Option<wl_buffer::WlBuffer>,
    /// Size the current `buffer` was created at, so a resize can be detected.
    size  : (u32, u32),
    /// Whether the compositor still holds it. Set on attach, cleared by
    /// `wl_buffer.release`.
    busy  : bool,
}

/// The candidate window.
pub struct Popup {
    /// The surface the pixels go on.
    surface   : wl_surface::WlSurface,
    /// The role object. It carries no requests but `destroy`; its whole
    /// purpose is to have been created, and to deliver `text_input_rectangle`.
    role      : ZwpInputPopupSurfaceV2,
    /// The shared-memory pool the buffers live in.
    pool      : wl_shm_pool::WlShmPool,
    /// The memfd behind the pool. Written with `pwrite`, never mapped: the
    /// renderer produces a pixmap anyway, so a mapping would only replace one
    /// copy with another and add the `unsafe` to go with it.
    memory    : File,
    /// The rotating buffers.
    slots     : [Slot; SLOTS],

    /// Fonts and glyph cache.
    renderer  : Renderer,
    /// Colours and metrics, read once (see [`super::theme`]).
    palette   : Palette,

    /// The candidate table the engine last sent, if any.
    table     : Option<LookupTable>,
    /// Whether the engine wants the table shown.
    table_shown: bool,
    /// The auxiliary text the engine last sent.
    aux       : Option<String>,
    /// Whether the engine wants it shown.
    aux_shown : bool,
    /// Whether a text field has focus. Nothing is drawn otherwise: the
    /// compositor would not show it, and it would not send frame callbacks
    /// either.
    active    : bool,

    /// Whether the content has changed since the last paint.
    dirty     : bool,
    /// Whether a frame callback is outstanding.
    throttled : bool,
    /// Which round of scheduling the outstanding callback belongs to. A
    /// callback cannot be cancelled, so a stale one is recognised rather than
    /// removed — the same trick key repeat uses.
    generation: u64,
    /// Whether a buffer is currently attached, i.e. whether the surface is
    /// mapped.
    mapped    : bool,
    /// The size of the last buffer committed, for the log line that says the
    /// popup is a plausible shape.
    last_size : (u32, u32),

    /// For creating buffers and callbacks.
    qh        : QueueHandle<Frontend>,
}

impl Popup {
    /// Creates the surface, gives it the popup role, and sets up the pool.
    ///
    /// Fallible because everything here is a resource acquisition that can
    /// genuinely fail — a memfd, a pool — and an input method that cannot draw
    /// candidates should still forward keys rather than refuse to start. The
    /// caller logs and carries on.
    pub fn new(
        compositor: &wl_compositor::WlCompositor,
        shm       : &wl_shm::WlShm,
        im        : &ZwpInputMethodV2,
        qh        : &QueueHandle<Frontend>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        use rustix::fs::{MemfdFlags, memfd_create};

        let memory = File::from(
            memfd_create("cosmic-voice-candidates", MemfdFlags::CLOEXEC)
                .context("memfd_create for the candidate buffers")?,
        );
        let total = SLOT_BYTES * SLOTS;
        memory
            .set_len(total as u64)
            .context("sizing the candidate buffer pool")?;

        let surface = compositor.create_surface(qh, ());
        let role = im.get_input_popup_surface(&surface, qh, ());
        let pool = shm.create_pool(memory.as_fd(), total as i32, qh, ());

        let palette = Palette::load();
        tracing::info!("candidate palette from {}", palette.source);

        Ok(Self {
            surface    : surface,
            role       : role,
            pool       : pool,
            memory     : memory,
            slots      : std::array::from_fn(|index| Slot {
                offset: index * SLOT_BYTES,
                buffer: None,
                size  : (0, 0),
                busy  : false,
            }),
            renderer   : Renderer::new(),
            palette    : palette,
            table      : None,
            table_shown: false,
            aux        : None,
            aux_shown  : false,
            active     : false,
            dirty      : false,
            throttled  : false,
            generation : 0,
            mapped     : false,
            last_size  : (0, 0),
            qh         : qh.clone(),
        })
    }

    /// Whether this surface is ours, so a dispatch can tell it from any other.
    fn owns(&self, surface: &wl_surface::WlSurface) -> bool {
        self.surface == *surface
    }
}

impl Drop for Popup {
    /// Tears the popup down in the order the protocol requires.
    ///
    /// The role object goes before the surface it wraps — "the client must not
    /// destroy the underlying `wl_surface` while the
    /// `zwp_input_popup_surface_v2` object exists"
    /// (`input-method-unstable-v2.xml:373-375`) — and the buffers go before the
    /// pool they were carved out of. In practice this only runs at process
    /// exit, where the connection closing would do the same thing; it exists so
    /// that the ordering is written down somewhere other than a comment.
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Some(buffer) = slot.buffer.take() {
                buffer.destroy();
            }
        }
        self.pool.destroy();
        self.role.destroy();
        self.surface.destroy();
    }
}

// --- Signals in ---

impl Popup {
    /// Applies one candidate-related signal, ignoring everything else.
    ///
    /// Everything the frontend does not handle itself comes here, so the match
    /// is total over what is left rather than over what is interesting.
    ///
    /// # Divergence from IBus's own panel
    ///
    /// `ShowLookupTable` and `ShowAuxiliaryText` are *applied* here. IBus's GTK
    /// panel ignores both — `ui/gtk3/panel.vala` overrides only the `update_*`
    /// and `hide_*` methods, so the base class routes the show requests to
    /// GObject signals nothing is connected to (`src/ibuspanelservice.c:1474`).
    /// That works for every engine that drives visibility through the `visible`
    /// flag on `Update*`, which is all of them in practice, and silently loses
    /// the one sequence that would need it: `update(table, false)` followed by
    /// `show()`. Honouring it costs a line and cannot make anything worse,
    /// since the daemon already suppresses a show that changes nothing
    /// (`bus/inputcontext.c:2364-2366`).
    pub fn on_signal(&mut self, signal: ContextSignal) {
        match signal {
            ContextSignal::UpdateLookupTable { table, visible } => {
                self.table = Some(table);
                self.table_shown = visible;
            }
            ContextSignal::ShowLookupTable => self.table_shown = true,
            ContextSignal::HideLookupTable => self.table_shown = false,
            ContextSignal::UpdateAuxiliary { text, visible } => {
                self.aux = Some(text.text);
                self.aux_shown = visible;
            }
            ContextSignal::ShowAuxiliary => self.aux_shown = true,
            ContextSignal::HideAuxiliary => self.aux_shown = false,
            ContextSignal::PageUpLookupTable     => self.move_cursor(Move::PageUp),
            ContextSignal::PageDownLookupTable   => self.move_cursor(Move::PageDown),
            ContextSignal::CursorUpLookupTable   => self.move_cursor(Move::Up),
            ContextSignal::CursorDownLookupTable => self.move_cursor(Move::Down),
            // Preedit, commits, surrounding text and engine properties: the
            // frontend deals with those, and this is where they end up because
            // it forwards everything it does not consume.
            _ => return,
        }

        self.refresh();
    }

    /// A text field took or lost focus.
    ///
    /// Losing it drops the table as well as unmapping, because the daemon
    /// itself pushes an empty table with `visible=false` on focus-out
    /// (`bus/inputcontext.c:2033`) and a candidate list from the previous field
    /// must never flash up in the next one.
    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        if !active {
            self.table = None;
            self.table_shown = false;
            self.aux = None;
            self.aux_shown = false;
        }

        self.refresh();
    }

    /// Where the compositor says the caret is, in the parent surface's
    /// coordinates.
    ///
    /// Logged and nothing else. cosmic-comp does the positioning itself —
    /// below the rectangle, clamped against the right edge, flipped above it if
    /// the bottom would overflow (`xdg_shell/popup.rs:178-205`) — so a client
    /// that also tried to anchor would be fighting it. It is worth having in
    /// the log because a rectangle that never arrives, or one that stays at the
    /// origin, is the difference between "the popup is misplaced" and "the
    /// application never told anyone where its caret is".
    fn on_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
        tracing::debug!("caret rectangle {width}x{height} at {x},{y}");
    }
}

// --- Cursor movement ---

/// One of the four cursor moves the daemon can ask for.
enum Move {
    /// Back one page.
    PageUp,
    /// Forward one page.
    PageDown,
    /// Back one candidate.
    Up,
    /// Forward one candidate.
    Down,
}

impl Popup {
    /// Moves the cursor the way ibus-daemon has already moved its own copy.
    ///
    /// `PageUpLookupTable` and friends carry **no payload** and the daemon does
    /// not re-send the table after one: it mutates its own `IBusLookupTable`
    /// (`bus/inputcontext.c:2425` and neighbours) and emits a bare signal. So a
    /// client with `CAP_LOOKUP_TABLE` has to mirror the arithmetic exactly, and
    /// "exactly" includes upstream's quirks — notably that a rounding page-up
    /// from the first page computes `page_count * page_size + slot` and then
    /// clamps, which always lands on the *last candidate* rather than on the
    /// same slot of the last page (`src/ibuslookuptable.c:434-451`). Being
    /// bug-compatible is the only way to stay in step with the daemon's cursor.
    fn move_cursor(&mut self, movement: Move) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        let total = table.candidates.len() as u32;
        let page_size = table.page_size.max(1);
        if total == 0 {
            return;
        }

        match movement {
            Move::PageUp if table.cursor_pos < page_size => {
                if !table.round {
                    return;
                }
                let slot = table.cursor_pos % page_size;
                let pages = total.div_ceil(page_size);
                table.cursor_pos = (pages * page_size + slot).min(total - 1);
            }
            Move::PageUp => table.cursor_pos -= page_size,
            Move::PageDown => {
                let slot = table.cursor_pos % page_size;
                let page = table.cursor_pos / page_size;
                let pages = total.div_ceil(page_size);
                if page + 1 == pages {
                    if !table.round {
                        return;
                    }
                    table.cursor_pos = slot;
                } else {
                    table.cursor_pos = (table.cursor_pos + page_size).min(total - 1);
                }
            }
            Move::Up => {
                table.cursor_pos = match table.cursor_pos {
                    0 if table.round => total - 1,
                    0                => return,
                    position         => position - 1,
                };
            }
            Move::Down => {
                table.cursor_pos = match table.cursor_pos {
                    position if position + 1 == total && table.round => 0,
                    position if position + 1 == total                => return,
                    position                                         => position + 1,
                };
            }
        }
    }
}

// --- What to draw ---

impl Popup {
    /// Turns the current state into a frame's worth of content.
    ///
    /// # The page
    ///
    /// `UpdateLookupTable` carries the **whole** candidate list — the daemon
    /// does no slicing (`bus/inputcontext.c:2764-2777`) — and `cursor_pos` is a
    /// global index into it, so the visible page is
    /// `[cursor_pos / page_size * page_size, +page_size)` and the highlighted
    /// row is `cursor_pos % page_size`. That is exactly what IBus's own panel
    /// computes (`ui/gtk3/candidatepanel.vala:339-361`).
    ///
    /// An engine using `ibus_engine_update_lookup_table_fast` sends a *window*
    /// of three pages rather than one, with `cursor_pos` renumbered to be
    /// window-relative (`src/ibusengine.c:2065-2117`). The same formula is
    /// correct for that too, which is why there is only one of it. The page
    /// counter is then relative to the window rather than to the whole list —
    /// cosmetic, and only reachable for lists of four pages or more.
    ///
    /// # The labels
    ///
    /// Indexed by slot within the page, not by candidate
    /// (`ui/gtk3/candidatepanel.vala:350-354`), and the defaults fill the
    /// remaining slots *absolutely* rather than continuing where the engine's
    /// left off (`ui/gtk3/candidatearea.vala:112-118`). Both are surprising and
    /// both are copied.
    fn view(&self) -> View {
        let aux = self
            .aux
            .as_ref()
            .filter(|_| self.aux_shown)
            .filter(|text| !text.is_empty())
            .cloned();
        let table = self.table.as_ref().filter(|_| self.table_shown);

        view_of(table, aux)
    }
}

/// The pure half of [`Popup::view`]: a table and an auxiliary line in, a
/// frame's content out.
///
/// Free rather than a method so the paging arithmetic — the part with all the
/// upstream quirks in it — can be tested without a Wayland connection.
fn view_of(table: Option<&LookupTable>, aux: Option<String>) -> View {
    let Some(table) = table else {
        return View {
            aux     : aux,
            vertical: true,
            ..View::default()
        };
    };

    let page_size = table.page_size.max(1) as usize;
    let total = table.candidates.len();
    let start = (table.cursor_pos as usize / page_size) * page_size;
    let end = (start + page_size).min(total);
    let rows = (start..end)
        .map(|index| Row {
            label: label_for(table, index - start),
            text : table.candidates[index].text.clone(),
        })
        .collect::<Vec<_>>();

    let pages = total.div_ceil(page_size);
    let cursor = table.cursor_pos as usize;

    View {
        cursor  : (table.cursor_visible && cursor >= start && cursor < end)
            .then(|| cursor - start),
        page    : (pages > 1).then(|| ((start / page_size + 1) as u32, pages as u32)),
        rows    : rows,
        aux     : aux,
        vertical: vertical(table),
    }
}

/// Resolves `IBUS_ORIENTATION_SYSTEM` to an actual orientation.
///
/// A fresh `IBusLookupTable` defaults to `SYSTEM`
/// (`src/ibuslookuptable.c:234`), so this is the common case rather than an
/// edge one. IBus's panel resolves it from the dconf key
/// `org.freedesktop.ibus.panel lookup-table-orientation`
/// (`ui/gtk3/panel.vala:808-815`), whose schema default is vertical, and whose
/// value on this machine is vertical. We do not read dconf: the frontend has no
/// dconf dependency, phase 4 will bring one for the engine-switch triggers, and
/// until then the schema default is what nearly every installation has.
fn vertical(table: &LookupTable) -> bool {
    match table.orientation {
        ORIENTATION_HORIZONTAL => false,
        ORIENTATION_VERTICAL   => true,
        ORIENTATION_SYSTEM     => true,
        // Not a value IBus defines. Vertical, like everything else that is not
        // explicitly horizontal.
        _                      => true,
    }
}

/// The label for one slot of a page.
fn label_for(table: &LookupTable, slot: usize) -> String {
    table
        .labels
        .get(slot)
        .map(|label: &Text| label.text.clone())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| DEFAULT_LABELS[slot.min(DEFAULT_LABELS.len() - 1)].to_string())
}

// --- Drawing ---

impl Popup {
    /// The content changed. Nothing is drawn yet; see [`Popup::flush`].
    fn refresh(&mut self) {
        self.dirty = true;
    }

    /// Draws whatever the accumulated state now says, once per event-loop pass.
    ///
    /// This is the coalescing point, and it is why [`Popup::refresh`] does not
    /// paint. One keystroke through mozc produces a burst of signals —
    /// `UpdateAuxiliaryText` then `UpdateLookupTable`, or `HideLookupTable`
    /// then `HideAuxiliaryText` — each of which arrives as its own callback on
    /// the signal channel. Painting from each one would attach a buffer for an
    /// intermediate state nobody should ever see: measured before this existed,
    /// committing a conversion drew a candidate-less window with only the
    /// auxiliary line in it, for the few microseconds between the two hide
    /// signals. Calloop's post-dispatch callback runs after the whole burst, so
    /// that state never reaches a buffer.
    pub fn flush(&mut self) {
        if !self.active || self.view().is_empty() {
            self.unmap();
            return;
        }

        self.paint();
    }

    /// Takes the surface down.
    ///
    /// A null-buffer commit is the only way to hide an input popup — the role
    /// has no request for it — and it is also the point at which the buffers
    /// can safely be destroyed, because nothing is attached any more. Doing so
    /// keeps the slot bookkeeping honest: a `release` that never arrives for an
    /// unmapped surface would otherwise leave a slot busy forever.
    fn unmap(&mut self) {
        if !self.mapped {
            return;
        }

        self.surface.attach(None, 0, 0);
        self.surface.commit();
        self.mapped = false;
        self.dirty = false;
        // Any outstanding frame callback belongs to a mapped surface and will
        // never arrive now, so the next paint must not wait for it.
        self.throttled = false;
        self.generation = self.generation.wrapping_add(1);
        for slot in &mut self.slots {
            if let Some(buffer) = slot.buffer.take() {
                buffer.destroy();
            }
            slot.busy = false;
            slot.size = (0, 0);
        }

        tracing::debug!("candidate window hidden");
    }

    /// Renders one frame, if one is due and the compositor is ready for it.
    fn paint(&mut self) {
        if !self.dirty || self.throttled || !self.active {
            return;
        }
        let Some(index) = self.slots.iter().position(|slot| !slot.busy) else {
            // Both buffers are still with the compositor. The release that
            // frees one comes back here, so this is a wait rather than a drop.
            tracing::debug!("candidate redraw waiting on a buffer release");
            return;
        };

        let view = self.view();
        let pixmap = self.renderer.render(&view, &self.palette);
        let (width, height) = (pixmap.width(), pixmap.height());
        if let Err(e) = self.upload(index, &pixmap) {
            tracing::warn!("could not fill a candidate buffer: {e:#}");
            return;
        }
        let Some(buffer) = self.buffer(index, width, height) else {
            return;
        };

        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage_buffer(0, 0, width as i32, height as i32);
        self.generation = self.generation.wrapping_add(1);
        self.surface.frame(&self.qh, self.generation);
        self.surface.commit();

        self.slots[index].busy = true;
        self.dirty = false;
        self.throttled = true;
        self.mapped = true;
        if self.last_size != (width, height) {
            tracing::info!(
                "candidate window {width}x{height}: {} candidates{}",
                view.rows.len(),
                if view.aux.is_some() { " + aux" } else { "" },
            );
            self.last_size = (width, height);
        }
    }

    /// Copies a rendered pixmap into a slot.
    ///
    /// `pwrite` rather than a mapping. The renderer hands back its own pixmap
    /// either way, so a mapping would replace one copy with another and add the
    /// `unsafe` block to go with it; at candidate-window sizes and rates a
    /// hundred-kilobyte write is not the cost that matters.
    fn upload(&self, index: usize, pixmap: &Pixmap) -> anyhow::Result<()> {
        use anyhow::Context as _;

        let stride = pixmap.width() as usize * 4;
        let bytes = stride * pixmap.height() as usize;
        anyhow::ensure!(
            bytes <= SLOT_BYTES,
            "a {}x{} candidate window needs {bytes} bytes, more than a slot holds",
            pixmap.width(),
            pixmap.height(),
        );

        let mut scratch = vec![0u8; bytes];
        render::to_argb8888(pixmap, &mut scratch, stride);
        self.memory
            .write_all_at(&scratch, self.slots[index].offset as u64)
            .context("writing the candidate buffer")?;

        Ok(())
    }

    /// The `wl_buffer` for a slot at a size, creating it if the size changed.
    fn buffer(&mut self, index: usize, width: u32, height: u32) -> Option<wl_buffer::WlBuffer> {
        let slot = &mut self.slots[index];
        if slot.size != (width, height)
            && let Some(buffer) = slot.buffer.take()
        {
            buffer.destroy();
        }

        if slot.buffer.is_none() {
            slot.buffer = Some(self.pool.create_buffer(
                slot.offset as i32,
                width as i32,
                height as i32,
                (width * 4) as i32,
                wl_shm::Format::Argb8888,
                &self.qh,
                index,
            ));
            slot.size = (width, height);
        }

        slot.buffer.clone()
    }

    /// The compositor is ready for another frame.
    fn on_frame(&mut self, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.throttled = false;
        self.paint();
    }

    /// The compositor is done with a buffer.
    fn on_release(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.busy = false;
        }
        self.paint();
    }
}

// --- Wayland dispatch ---
//
// These live here rather than in `frontend` so that the whole candidate window
// is one file. They reach the popup through `Frontend::popup`, which is the
// only thing the frontend has to expose for any of this to work.

impl Dispatch<ZwpInputPopupSurfaceV2, ()> for Frontend {
    fn event(
        frontend: &mut Self,
        _: &ZwpInputPopupSurfaceV2,
        event: zwp_input_popup_surface_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_input_popup_surface_v2::Event::TextInputRectangle { x, y, width, height } = event
            && let Some(popup) = frontend.popup()
        {
            popup.on_rectangle(x, y, width, height);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for Frontend {
    fn event(
        frontend: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        generation: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && let Some(popup) = frontend.popup()
        {
            popup.on_frame(*generation);
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for Frontend {
    fn event(
        frontend: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(popup) = frontend.popup()
        {
            popup.on_release(*index);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Frontend {
    fn event(
        frontend: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `enter` and `leave` name the outputs the popup is on, which is how a
        // client learns the scale it should be drawing at. We draw at scale 1
        // and let the compositor scale the result; every output on the machine
        // this was built for is at scale 1, and doing it properly means
        // tracking `wl_output` and repainting on a move. Logged so that the day
        // it looks blurry there is a line saying why.
        if let wl_surface::Event::Enter { output } = event
            && frontend.popup().is_some_and(|popup| popup.owns(surface))
        {
            tracing::debug!("candidate window entered output {}", output.id());
        }
    }
}

wayland_client::delegate_noop!(Frontend: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Frontend: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Frontend: ignore wl_shm_pool::WlShmPool);

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A table with `count` numbered candidates, a page size and a cursor.
    fn table(count: u32, page_size: u32, cursor: u32) -> LookupTable {
        LookupTable {
            page_size     : page_size,
            cursor_pos    : cursor,
            cursor_visible: true,
            round         : false,
            orientation   : ORIENTATION_VERTICAL,
            candidates    : (0..count).map(|n| Text::plain(format!("c{n}"))).collect(),
            labels        : Vec::new(),
        }
    }

    /// The whole list arrives and the page is derived from the cursor. Getting
    /// this wrong shows candidates 1-9 while the engine thinks the user is
    /// looking at 10-18.
    #[test]
    fn derives_the_page_from_the_cursor() {
        let view = view_of(Some(&table(20, 9, 11)), None);

        assert_eq!(view.rows.len(), 9);
        assert_eq!(view.rows[0].text, "c9");
        assert_eq!(view.cursor, Some(2));
        assert_eq!(view.page, Some((2, 3)));
    }

    /// The last page is short, and asking for a full one past the end is how a
    /// renderer panics on a real conversion.
    #[test]
    fn clamps_the_last_page() {
        let view = view_of(Some(&table(20, 9, 19)), None);

        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[1].text, "c19");
        assert_eq!(view.cursor, Some(1));
    }

    /// A hidden cursor means no highlight, which is how an engine offers a list
    /// it is not yet selecting within.
    #[test]
    fn draws_no_focus_when_the_cursor_is_hidden() {
        let mut table = table(5, 5, 2);
        table.cursor_visible = false;

        assert_eq!(view_of(Some(&table), None).cursor, None);
    }

    /// One page is no page counter: a `1/1` on every conversion is noise.
    #[test]
    fn counts_pages_only_when_there_is_more_than_one() {
        assert_eq!(view_of(Some(&table(5, 9, 0)), None).page, None);
        assert_eq!(view_of(Some(&table(10, 9, 0)), None).page, Some((1, 2)));
    }

    /// Labels come from the engine per *slot*; the ones it does not supply come
    /// from IBus's table indexed absolutely, not continued from where the
    /// engine's stopped.
    #[test]
    fn fills_missing_labels_absolutely() {
        let mut table = table(5, 5, 0);
        table.labels = vec![Text::plain("あ"), Text::plain("い")];
        let view = view_of(Some(&table), None);

        assert_eq!(view.rows[0].label, "あ");
        assert_eq!(view.rows[1].label, "い");
        assert_eq!(view.rows[2].label, "3.");
        assert_eq!(view.rows[4].label, "5.");
    }

    /// The tenth default label is `0.`, not `10.`, because that is the key you
    /// press.
    #[test]
    fn labels_the_tenth_candidate_zero() {
        assert_eq!(view_of(Some(&table(10, 10, 0)), None).rows[9].label, "0.");
    }

    /// Auxiliary text with no candidates is a window of its own, the way IBus's
    /// panel renders mozc's mode hint while kana are still being typed.
    #[test]
    fn shows_auxiliary_text_alone() {
        let view = view_of(None, Some("ひらがな".to_string()));

        assert!(!view.is_empty());
        assert!(view.rows.is_empty());
    }

    /// Nothing at all is nothing, and the popup answers that by unmapping.
    #[test]
    fn nothing_to_show_is_empty() {
        assert!(view_of(None, None).is_empty());
    }

    /// `SYSTEM` is what a fresh `IBusLookupTable` carries, so it is the value
    /// that actually decides the layout for most engines.
    #[test]
    fn resolves_system_orientation_to_vertical() {
        let mut table = table(3, 3, 0);
        table.orientation = ORIENTATION_SYSTEM;
        assert!(vertical(&table));

        table.orientation = ORIENTATION_HORIZONTAL;
        assert!(!vertical(&table));
    }
}
