//! The engine's status menu, as a model the popup can be drawn from.
//!
//! `ibus-ui-gtk3` kept this in its tray indicator: the mode glyph beside the
//! engine icon, and a menu of the engine's properties behind it. Both come
//! from the same two signals — `RegisterProperties` replaces the whole list,
//! `UpdateProperty` changes one entry in place — and this is the state those
//! two signals maintain, with nothing about D-Bus, Wayland or iced in it, for
//! the same reason [`super::router`] and [`super::dictation`] are pure: the
//! tests at the bottom run on `cargo test` and the rest needs a daemon.
//!
//! # The two rules
//!
//! **The indicator is one property's `symbol`.** The engine's description
//! names a property key (`icon_prop_key`, `InputMode` for mozc) and the glyph
//! is the `symbol` of the registered property with that key
//! (`ui/gtk3/panel.vala:1790-1795`), refreshed by every `UpdateProperty` that
//! names it. An engine with no such key, or a property with no symbol, has no
//! indicator; the applet falls back to the engine's static symbol.
//!
//! **An update matches by key, at any depth.** mozc keeps its input modes as
//! radio children of the `InputMode` menu and updates them one by one when the
//! mode changes, then the menu itself with the new label and symbol
//! (`unix/ibus/property_handler.cc`, `UpdateCompositionModeIcon`). An update
//! for a key that was never registered is dropped: the panel does the same,
//! and inventing a top-level entry for it would draw a menu the engine never
//! described.

use crate::ibus::{
    PROP_STATE_CHECKED, PROP_STATE_INCONSISTENT, PROP_STATE_UNCHECKED, PROP_TYPE_MENU,
    PROP_TYPE_NORMAL, PROP_TYPE_RADIO, PROP_TYPE_SEPARATOR, PROP_TYPE_TOGGLE, PropList, Property,
};
use crate::ipc::{ImPropKind, ImPropState, ImProperty};

// --- The model ---

/// The registered status menu of the engine in effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Properties {
    /// The top-level entries, in the engine's order. Submenus hang off them.
    list: PropList,
}

impl Properties {
    /// No menu, which is what an engine that registered nothing has.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the whole menu. Returns whether anything changed, so the
    /// caller can skip publishing a snapshot nobody would see a difference in
    /// — mozc re-registers an identical list on every focus change.
    pub fn register(&mut self, list: PropList) -> bool {
        if self.list == list {
            return false;
        }
        self.list = list;

        true
    }

    /// Changes one entry in place, matched by key at any depth.
    ///
    /// Returns whether an entry with that key was found; the update is
    /// dropped otherwise. The whole property is replaced rather than merged
    /// because that is what the daemon does (`ibus_prop_list_update_property`
    /// copies the new object over the old one wholesale), and a merge would
    /// keep children an engine meant to remove.
    pub fn update(&mut self, prop: Property) -> bool {
        update_in(&mut self.list, prop).is_none()
    }

    /// Forgets the menu, for an engine change with no registration behind it.
    pub fn clear(&mut self) {
        self.list = PropList::default();
    }

    /// The mode glyph: the `symbol` of the property whose key is the engine's
    /// `icon_prop_key`, if there is one and it is not empty.
    pub fn indicator(&self, icon_prop_key: &str) -> Option<String> {
        if icon_prop_key.is_empty() {
            return None;
        }

        let mut symbol = None;
        for property in &self.list.properties {
            property.walk(&mut |property| {
                if symbol.is_none() && property.key == icon_prop_key {
                    symbol = Some(property.symbol.text.clone());
                }
            });
        }

        symbol.filter(|symbol| !symbol.is_empty())
    }

    /// The menu as the popup draws it: every top-level entry, with its
    /// children, in the engine's order. Visibility and sensitivity travel
    /// with the entries rather than being filtered here, so the popup can
    /// grey out what the engine greyed out.
    pub fn menu(&self) -> Vec<ImProperty> {
        self.list.properties.iter().map(to_ipc).collect()
    }
}

/// The recursive half of [`Properties::update`]: returns the property back if
/// nothing in `list` matched its key, so the caller can keep looking.
fn update_in(list: &mut PropList, prop: Property) -> Option<Property> {
    let mut prop = Some(prop);
    for entry in &mut list.properties {
        let candidate = prop.take()?;
        if entry.key == candidate.key {
            *entry = candidate;
            return None;
        }
        prop = update_in(&mut entry.sub_props, candidate);
    }

    prop
}

/// Translates one entry into the serde form the applet renders.
///
/// A label the engine left empty becomes the key, which is at least something
/// a person can act on; an unknown type or state becomes the conservative
/// reading (`Normal`, `Unchecked`) rather than an error, because a menu entry
/// we cannot draw is not a reason to drop the menu.
fn to_ipc(property: &Property) -> ImProperty {
    let kind = match property.kind {
        PROP_TYPE_TOGGLE    => ImPropKind::Toggle,
        PROP_TYPE_RADIO     => ImPropKind::Radio,
        PROP_TYPE_MENU      => ImPropKind::Menu,
        PROP_TYPE_SEPARATOR => ImPropKind::Separator,
        PROP_TYPE_NORMAL    => ImPropKind::Normal,
        other               => {
            tracing::debug!("property {} has unknown type {other}; drawing it as normal", property.key);
            ImPropKind::Normal
        }
    };
    let state = match property.state {
        PROP_STATE_CHECKED      => ImPropState::Checked,
        PROP_STATE_INCONSISTENT => ImPropState::Inconsistent,
        PROP_STATE_UNCHECKED    => ImPropState::Unchecked,
        other                   => {
            tracing::debug!("property {} has unknown state {other}; drawing it unchecked", property.key);
            ImPropState::Unchecked
        }
    };
    let label = if property.label.text.is_empty() {
        property.key.clone()
    } else {
        property.label.text.clone()
    };

    ImProperty {
        key      : property.key.clone(),
        label    : label,
        kind     : kind,
        state    : state,
        sensitive: property.sensitive,
        visible  : property.visible,
        symbol   : property.symbol.text.clone(),
        children : property.sub_props.properties.iter().map(to_ipc).collect(),
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ibus::Text;

    /// A property with everything but the key, type and label defaulted.
    fn prop(key: &str, kind: u32, label: &str) -> Property {
        Property {
            key      : key.to_string(),
            kind     : kind,
            label    : Text::plain(label),
            icon     : String::new(),
            tooltip  : Text::default(),
            sensitive: true,
            visible  : true,
            state    : PROP_STATE_UNCHECKED,
            sub_props: PropList::default(),
            symbol   : Text::default(),
        }
    }

    /// A list of the given entries.
    fn list(properties: Vec<Property>) -> PropList {
        PropList {
            properties: properties,
        }
    }

    /// What mozc registers on `FocusIn`, in hiragana mode.
    fn mozc_list() -> PropList {
        let mut input_mode = prop("InputMode", PROP_TYPE_MENU, "Input Mode (あ)");
        input_mode.symbol = Text::plain("あ");
        let mut hiragana = prop("InputMode.Hiragana", PROP_TYPE_RADIO, "Hiragana");
        hiragana.state = PROP_STATE_CHECKED;
        input_mode.sub_props = list(vec![
            prop("InputMode.Direct", PROP_TYPE_RADIO, "Direct input"),
            hiragana,
            prop("InputMode.Katakana", PROP_TYPE_RADIO, "Katakana"),
        ]);

        let mut tool = prop("MozcTool", PROP_TYPE_MENU, "Tools");
        tool.sub_props = list(vec![prop(
            "Tool.DictionaryTool",
            PROP_TYPE_NORMAL,
            "Dictionary Tool",
        )]);

        list(vec![input_mode, tool])
    }

    /// The burst mozc sends when the mode changes to direct input: each radio
    /// child with its new state, then the menu with its new symbol.
    fn switch_to_direct(model: &mut Properties) {
        let mut direct = prop("InputMode.Direct", PROP_TYPE_RADIO, "Direct input");
        direct.state = PROP_STATE_CHECKED;
        assert!(model.update(direct));
        let mut hiragana = prop("InputMode.Hiragana", PROP_TYPE_RADIO, "Hiragana");
        hiragana.state = PROP_STATE_UNCHECKED;
        assert!(model.update(hiragana));
        let mut input_mode = prop("InputMode", PROP_TYPE_MENU, "Input Mode (A)");
        input_mode.symbol = Text::plain("A");
        // mozc sends the menu with its children attached, as every
        // `IBusProperty` carries its `sub_props`; the replacement has to keep
        // them, or the group vanishes on every mode change.
        input_mode.sub_props = list(vec![
            {
                let mut prop = prop("InputMode.Direct", PROP_TYPE_RADIO, "Direct input");
                prop.state = PROP_STATE_CHECKED;
                prop
            },
            prop("InputMode.Hiragana", PROP_TYPE_RADIO, "Hiragana"),
            prop("InputMode.Katakana", PROP_TYPE_RADIO, "Katakana"),
        ]);
        assert!(model.update(input_mode));
    }

    /// The glyph is the symbol of the property named by the engine, and
    /// nothing else in the list.
    #[test]
    fn indicator_is_the_icon_property_symbol() {
        let mut model = Properties::new();
        model.register(mozc_list());
        assert_eq!(model.indicator("InputMode"), Some("あ".to_string()));
        assert_eq!(model.indicator("MozcTool"), None);
        assert_eq!(model.indicator("Nonsuch"), None);
    }

    /// An xkb engine has no `icon_prop_key`, and asking with an empty one
    /// must not match a property with an empty key by accident.
    #[test]
    fn no_icon_key_means_no_indicator() {
        let mut model = Properties::new();
        model.register(mozc_list());
        assert_eq!(model.indicator(""), None);
    }

    /// A mode change arrives as updates, and the indicator follows the last
    /// of them.
    #[test]
    fn updates_move_the_indicator_and_the_check() {
        let mut model = Properties::new();
        model.register(mozc_list());
        switch_to_direct(&mut model);

        assert_eq!(model.indicator("InputMode"), Some("A".to_string()));
        let menu = model.menu();
        assert_eq!(menu[0].label, "Input Mode (A)");
        assert_eq!(menu[0].children[0].state, ImPropState::Checked);
        assert_eq!(menu[0].children[1].state, ImPropState::Unchecked);
        assert_eq!(menu[0].children.len(), 3);
    }

    /// An update names a child two levels down, and the matching is by key
    /// rather than by position: the entry moves nowhere.
    #[test]
    fn update_reaches_into_submenus_by_key() {
        let mut model = Properties::new();
        model.register(mozc_list());
        let mut tool = prop("Tool.DictionaryTool", PROP_TYPE_NORMAL, "Dictionary");
        tool.sensitive = false;
        assert!(model.update(tool));

        let menu = model.menu();
        assert_eq!(menu[1].children[0].label, "Dictionary");
        assert!(!menu[1].children[0].sensitive);
        assert_eq!(menu[0].children.len(), 3);
    }

    /// The panel drops an update for a key it never saw, and so do we.
    #[test]
    fn update_for_an_unknown_key_is_dropped() {
        let mut model = Properties::new();
        model.register(mozc_list());
        let before = model.menu();
        assert!(!model.update(prop("Nonsuch", PROP_TYPE_NORMAL, "?")));
        assert_eq!(model.menu(), before);
    }

    /// A registration replaces everything, an identical one changes nothing,
    /// and clearing leaves no menu and no indicator.
    #[test]
    fn register_replaces_and_clear_empties() {
        let mut model = Properties::new();
        assert!(model.menu().is_empty());
        assert!(model.register(mozc_list()));
        assert!(!model.register(mozc_list()));
        assert!(!model.menu().is_empty());

        assert!(model.register(list(vec![prop(
            "Other",
            PROP_TYPE_TOGGLE,
            "Other",
        )])));
        assert_eq!(model.menu().len(), 1);
        assert_eq!(model.menu()[0].kind, ImPropKind::Toggle);
        assert_eq!(model.indicator("InputMode"), None);

        model.clear();
        assert!(model.menu().is_empty());
    }

    /// The serde form keeps what the popup draws and substitutes the key for
    /// a missing label, so every row has text.
    #[test]
    fn menu_view_translates_kinds_states_and_labels() {
        let mut model = Properties::new();
        let mut sep = prop("sep", PROP_TYPE_SEPARATOR, "");
        sep.visible = false;
        let mut unlabelled = prop("Tool.About", PROP_TYPE_NORMAL, "");
        unlabelled.state = PROP_STATE_INCONSISTENT;
        model.register(list(vec![sep, unlabelled]));

        let menu = model.menu();
        assert_eq!(menu[0].kind, ImPropKind::Separator);
        assert!(!menu[0].visible);
        assert_eq!(menu[1].label, "Tool.About");
        assert_eq!(menu[1].state, ImPropState::Inconsistent);
        assert_eq!(menu[1].state.as_u32(), 2);
    }
}
