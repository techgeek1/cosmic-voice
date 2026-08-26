//! Focused window tracking, used to choose a vocabulary prompt.
//!
//! `ext_foreign_toplevel_list_v1` enumerates toplevels and
//! `zcosmic_toplevel_info_v1` reports which one is activated. Knowing the
//! focused app_id is what lets the same keypress dictate graphics and compiler
//! jargon into a terminal and ordinary prose into a browser.

use anyhow::Result;

/// Tracks which toplevel currently has focus.
pub struct FocusTracker {
    /// app_id of the activated toplevel, if any.
    focused : Option<String>,
}

// --- FocusTracker ---

impl FocusTracker {
    /// Starts tracking on its own Wayland connection.
    pub fn connect() -> Result<Self> {
        todo!("bind ext_foreign_toplevel_list_v1 and zcosmic_toplevel_info_v1")
    }

    /// Returns the focused app_id, or `None` if nothing is activated.
    ///
    /// Sampled when an utterance *begins*, not when it ends. Focus can move
    /// while the user is still speaking, and the prompt should match what they
    /// were looking at when they started.
    pub fn focused_app_id(&self) -> Option<&str> {
        self.focused.as_deref()
    }
}
