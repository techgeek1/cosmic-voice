//! The `IBusSerializable` codec.
//!
//! Every non-trivial payload IBus puts on the wire is a GObject serialised by
//! hand into a D-Bus structure, not a plain D-Bus type. The scheme is uniform
//! and comes from `ibus_serializable_serialize()`: the first field is the
//! GType name as a string, the second an `a{sv}` of "attachments" (an
//! extension slot nothing in the daemon path actually uses), and the object's
//! own fields follow positionally, subclass fields appended after superclass
//! ones. Deserialisation is by position, so field *order* is the contract —
//! upstream says as much in a comment on `IBusEngineDesc`
//! (`src/ibusenginedesc.c`: "you should not change the serialized order …
//! because the order is also used in other applications likes ibus-qt").
//!
//! The five types below are the ones the multiplexer needs. Their layouts were
//! read out of ibus 1.5.34's `serialize()` implementations rather than guessed
//! from introspection, because introspection only ever says `v`:
//!
//! | type              | signature                    | source                    |
//! | ----------------- | ---------------------------- | ------------------------- |
//! | `IBusText`        | `(sa{sv}sv)`                 | `src/ibustext.c`          |
//! | `IBusAttrList`    | `(sa{sv}av)`                 | `src/ibusattrlist.c`      |
//! | `IBusAttribute`   | `(sa{sv}uuuu)`               | `src/ibusattribute.c`     |
//! | `IBusLookupTable` | `(sa{sv}uubbiavav)`          | `src/ibuslookuptable.c`   |
//! | `IBusEngineDesc`  | `(sa{sv}ssssssssussssssss)`  | `src/ibusenginedesc.c`    |
//!
//! `IBusProperty` and `IBusPropList` (the `RegisterProperties` and
//! `UpdateProperty` signals) are deliberately *not* decoded. They describe the
//! engine's status-icon menu, which belongs to the panel applet's surface and
//! not to the IM path; phase 4 can add them when there is something to render
//! them into. Until then those signals carry their payload through as an
//! undecoded value.

use std::collections::HashMap;

use zbus::zvariant::{StructureBuilder, Value};

use super::{Error, Result};

// --- The serialisation scheme ---

/// A type that travels as an `IBusSerializable` structure.
///
/// The header (type name plus attachments) is handled once, here, so that the
/// implementations below are only ever the positional field list — which is
/// what has to be checked against upstream when a version changes.
pub trait Serializable: Sized {
    /// The GType name in field 0. The daemon dispatches deserialisation on it,
    /// and we verify it on the way in: a mismatch means the engine sent
    /// something other than what the signal signature promised, and decoding
    /// the fields anyway would silently produce nonsense.
    const TYPE_NAME: &'static str;

    /// Appends this object's own fields, in wire order, after the header.
    fn encode(&self, builder: StructureBuilder<'static>) -> Result<StructureBuilder<'static>>;

    /// Reads this object's own fields back. `fields` is the *whole* structure
    /// including the two header fields, so indices here match the positions in
    /// the signature table above and can be read against upstream directly.
    fn decode(fields: &[Value<'_>]) -> Result<Self>;

    /// Wraps [`Serializable::encode`] in the header every IBus object carries.
    fn to_value(&self) -> Result<Value<'static>> {
        let builder = StructureBuilder::new()
            .add_field(Self::TYPE_NAME.to_string())
            .add_field(HashMap::<String, Value<'static>>::new());

        let structure = self
            .encode(builder)?
            .build()
            .map_err(|e| Error::Decode {
                what  : Self::TYPE_NAME,
                reason: format!("building the structure: {e}"),
            })?;

        Ok(Value::Structure(structure))
    }

    /// Checks the header and hands the fields to [`Serializable::decode`].
    fn from_value(value: &Value<'_>) -> Result<Self> {
        let structure = match unwrap_variant(value) {
            Value::Structure(structure) => structure,
            other => {
                return Err(Error::Decode {
                    what  : Self::TYPE_NAME,
                    reason: format!("expected a structure, got {}", other.value_signature()),
                });
            }
        };

        let fields = structure.fields();
        let name = field_str(fields, 0, Self::TYPE_NAME)?;
        if name != Self::TYPE_NAME {
            return Err(Error::Decode {
                what  : Self::TYPE_NAME,
                reason: format!("type name is {name:?}"),
            });
        }

        Self::decode(fields)
    }
}

// --- Field accessors ---

/// Peels one level of variant.
///
/// A `v` field decodes to the contained value directly in most paths, but
/// nested variants (`av` elements, a variant inside a variant) can arrive
/// still boxed. Peeling unconditionally costs nothing and removes an entire
/// class of "works for commits, fails for candidates" bug.
fn unwrap_variant<'v>(value: &'v Value<'v>) -> &'v Value<'v> {
    match value {
        Value::Value(inner) => inner,
        other               => other,
    }
}

/// Fetches field `index`, reporting the type being decoded when it is absent.
fn field<'v>(fields: &'v [Value<'v>], index: usize, what: &'static str) -> Result<&'v Value<'v>> {
    fields.get(index).ok_or_else(|| Error::Decode {
        what  : what,
        reason: format!("field {index} missing, only {} present", fields.len()),
    })
}

/// Reads a `s` field.
fn field_str(fields: &[Value<'_>], index: usize, what: &'static str) -> Result<String> {
    match field(fields, index, what)? {
        Value::Str(s) => Ok(s.to_string()),
        other         => Err(mistyped(what, index, "s", other)),
    }
}

/// Reads a `u` field.
fn field_u32(fields: &[Value<'_>], index: usize, what: &'static str) -> Result<u32> {
    match field(fields, index, what)? {
        Value::U32(v) => Ok(*v),
        other         => Err(mistyped(what, index, "u", other)),
    }
}

/// Reads an `i` field.
fn field_i32(fields: &[Value<'_>], index: usize, what: &'static str) -> Result<i32> {
    match field(fields, index, what)? {
        Value::I32(v) => Ok(*v),
        other         => Err(mistyped(what, index, "i", other)),
    }
}

/// Reads a `b` field.
fn field_bool(fields: &[Value<'_>], index: usize, what: &'static str) -> Result<bool> {
    match field(fields, index, what)? {
        Value::Bool(v) => Ok(*v),
        other          => Err(mistyped(what, index, "b", other)),
    }
}

/// Reads an `av` field as a list of serialisable objects.
fn field_objects<T: Serializable>(
    fields: &[Value<'_>],
    index : usize,
    what  : &'static str,
) -> Result<Vec<T>> {
    match field(fields, index, what)? {
        Value::Array(array) => array.iter().map(T::from_value).collect(),
        other               => Err(mistyped(what, index, "av", other)),
    }
}

/// Builds the "field n was not a u after all" error the accessors share.
fn mistyped(what: &'static str, index: usize, expected: &str, got: &Value<'_>) -> Error {
    Error::Decode {
        what  : what,
        reason: format!(
            "field {index} should be {expected}, got {}",
            got.value_signature()
        ),
    }
}

/// Turns a list of serialisable objects into the `av` the wire wants.
fn objects_to_array<T: Serializable>(objects: &[T]) -> Result<Vec<Value<'static>>> {
    objects.iter().map(T::to_value).collect()
}

// --- IBusAttribute ---

/// Decoration attribute type, `IBusAttrType` (ibusattribute.h:78-82).
pub const ATTR_UNDERLINE: u32 = 1;
/// Foreground colour, value is `0xRRGGBB` (ibusattribute.h:79).
pub const ATTR_FOREGROUND: u32 = 2;
/// Background colour, value is `0xRRGGBB` (ibusattribute.h:80).
pub const ATTR_BACKGROUND: u32 = 3;
/// Preedit hint; engines that do not care may ignore it (ibusattribute.h:81).
pub const ATTR_HINT: u32 = 4;

/// One decoration span over an [`Text`].
///
/// These are what make mozc's preedit legible: the converted segment carries a
/// background attribute and the segment under the cursor a different one, so a
/// client that drops them shows an undifferentiated run of kana.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    /// One of the `ATTR_*` constants.
    pub kind : u32,
    /// Meaning depends on `kind`: an `IBusAttrUnderline` for underlines, a
    /// packed RGB for colours.
    pub value: u32,
    /// First character covered. **Unicode character offsets, not bytes** —
    /// converting to byte offsets is the client's job and is where CJK
    /// preedit rendering usually goes wrong.
    pub start: u32,
    /// One past the last character covered.
    pub end  : u32,
}

impl Serializable for Attribute {
    const TYPE_NAME: &'static str = "IBusAttribute";

    fn encode(&self, builder: StructureBuilder<'static>) -> Result<StructureBuilder<'static>> {
        Ok(builder
            .add_field(self.kind)
            .add_field(self.value)
            .add_field(self.start)
            .add_field(self.end))
    }

    fn decode(fields: &[Value<'_>]) -> Result<Self> {
        Ok(Self {
            kind : field_u32(fields, 2, Self::TYPE_NAME)?,
            value: field_u32(fields, 3, Self::TYPE_NAME)?,
            start: field_u32(fields, 4, Self::TYPE_NAME)?,
            end  : field_u32(fields, 5, Self::TYPE_NAME)?,
        })
    }
}

impl std::fmt::Display for Attribute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            ATTR_UNDERLINE  => "underline",
            ATTR_FOREGROUND => "foreground",
            ATTR_BACKGROUND => "background",
            ATTR_HINT       => "hint",
            _               => "unknown",
        };
        write!(f, "{kind}({}) {}..{}", self.value, self.start, self.end)
    }
}

// --- IBusAttrList ---

/// The attribute list attached to every [`Text`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttrList {
    /// Spans in the order the engine emitted them; they may overlap.
    pub attributes: Vec<Attribute>,
}

impl Serializable for AttrList {
    const TYPE_NAME: &'static str = "IBusAttrList";

    fn encode(&self, builder: StructureBuilder<'static>) -> Result<StructureBuilder<'static>> {
        Ok(builder.add_field(objects_to_array(&self.attributes)?))
    }

    fn decode(fields: &[Value<'_>]) -> Result<Self> {
        Ok(Self {
            attributes: field_objects(fields, 2, Self::TYPE_NAME)?,
        })
    }
}

// --- IBusText ---

/// A string plus its decoration, the currency of the whole protocol.
///
/// Commits, preedit, auxiliary text and every candidate in a lookup table are
/// an `IBusText`. So, less obviously, are the non-text records in the
/// post-process-key drain: the daemon has no other envelope to hand, so it
/// formats numbers into one (see [`super::context::PostRecord`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Text {
    /// The text itself.
    pub text : String,
    /// Decoration spans, empty for a plain commit.
    pub attrs: AttrList,
}

impl Text {
    /// An undecorated text, which is what we send and most of what we get.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text : text.into(),
            attrs: AttrList::default(),
        }
    }
}

impl Serializable for Text {
    const TYPE_NAME: &'static str = "IBusText";

    fn encode(&self, builder: StructureBuilder<'static>) -> Result<StructureBuilder<'static>> {
        // `append_field` rather than `add_field`: the latter re-wraps anything
        // whose signature is already `v` (zvariant `structure.rs`, via
        // `Value::new`), which would produce a variant inside a variant. The
        // attribute list is a `v` field, so it has to go in verbatim.
        Ok(builder
            .add_field(self.text.clone())
            .append_field(Value::Value(Box::new(self.attrs.to_value()?))))
    }

    fn decode(fields: &[Value<'_>]) -> Result<Self> {
        Ok(Self {
            text : field_str(fields, 2, Self::TYPE_NAME)?,
            attrs: AttrList::from_value(field(fields, 3, Self::TYPE_NAME)?)?,
        })
    }
}

impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.text)?;
        if !self.attrs.attributes.is_empty() {
            let spans: Vec<String> = self
                .attrs
                .attributes
                .iter()
                .map(Attribute::to_string)
                .collect();
            write!(f, " [{}]", spans.join(", "))?;
        }

        Ok(())
    }
}

// --- IBusLookupTable ---

/// Candidates run left to right (ibustypes.h:151).
pub const ORIENTATION_HORIZONTAL: i32 = 0;
/// Candidates run top to bottom (ibustypes.h:152).
pub const ORIENTATION_VERTICAL: i32 = 1;
/// Whatever IBus is globally configured for (ibustypes.h:153).
pub const ORIENTATION_SYSTEM: i32 = 2;

/// The candidate window's contents.
///
/// Phase 3 renders this into an input-popup surface. It arrives here already
/// because declaring `CAP_LOOKUP_TABLE` is what makes the daemon send it to us
/// instead of to a panel process, and the whole point of the multiplexer is
/// that there is no longer a panel process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupTable {
    /// Candidates shown at once. Paging is the engine's business, not ours.
    pub page_size      : u32,
    /// Index of the selected candidate, absolute across pages.
    pub cursor_pos     : u32,
    /// Whether the selection should be drawn at all.
    pub cursor_visible : bool,
    /// Whether moving past the end wraps.
    pub round          : bool,
    /// One of the `ORIENTATION_*` constants.
    pub orientation    : i32,
    /// The candidates, in the engine's order.
    pub candidates     : Vec<Text>,
    /// Per-candidate labels ("1.", "2.", …). Empty means "use the default".
    pub labels         : Vec<Text>,
}

impl Serializable for LookupTable {
    const TYPE_NAME: &'static str = "IBusLookupTable";

    fn encode(&self, builder: StructureBuilder<'static>) -> Result<StructureBuilder<'static>> {
        Ok(builder
            .add_field(self.page_size)
            .add_field(self.cursor_pos)
            .add_field(self.cursor_visible)
            .add_field(self.round)
            .add_field(self.orientation)
            .add_field(objects_to_array(&self.candidates)?)
            .add_field(objects_to_array(&self.labels)?))
    }

    fn decode(fields: &[Value<'_>]) -> Result<Self> {
        Ok(Self {
            page_size     : field_u32(fields, 2, Self::TYPE_NAME)?,
            cursor_pos    : field_u32(fields, 3, Self::TYPE_NAME)?,
            cursor_visible: field_bool(fields, 4, Self::TYPE_NAME)?,
            round         : field_bool(fields, 5, Self::TYPE_NAME)?,
            orientation   : field_i32(fields, 6, Self::TYPE_NAME)?,
            candidates    : field_objects(fields, 7, Self::TYPE_NAME)?,
            labels        : field_objects(fields, 8, Self::TYPE_NAME)?,
        })
    }
}

impl std::fmt::Display for LookupTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let orientation = match self.orientation {
            ORIENTATION_HORIZONTAL => "horizontal",
            ORIENTATION_VERTICAL   => "vertical",
            ORIENTATION_SYSTEM     => "system",
            _                      => "unknown",
        };
        write!(
            f,
            "{} candidates, page {}, cursor {}{}, {orientation}{}",
            self.candidates.len(),
            self.page_size,
            self.cursor_pos,
            if self.cursor_visible { "" } else { " (hidden)" },
            if self.round { ", round" } else { "" },
        )?;
        for (index, candidate) in self.candidates.iter().enumerate() {
            let label = self
                .labels
                .get(index)
                .map(|label| label.text.clone())
                .unwrap_or_else(|| format!("{}.", index + 1));
            write!(f, "\n    {label} {candidate}")?;
        }

        Ok(())
    }
}

// --- IBusEngineDesc ---

/// An engine's registry entry: what `GlobalEngine`, `ActiveEngines` and
/// `GetEnginesByNames` are all made of.
///
/// Seventeen positional fields, and the order is frozen by upstream policy
/// (`src/ibusenginedesc.c`, `ibus_engine_desc_serialize`). `rank` sits in the
/// middle rather than at the end because it was there before the second batch
/// of strings was appended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineDesc {
    /// Engine id, e.g. `mozc-jp` or `xkb:us::eng`. What `SetEngine` takes.
    pub name          : String,
    /// Human-readable name for a UI.
    pub longname      : String,
    /// Longer description.
    pub description   : String,
    /// Language code the engine inputs.
    pub language      : String,
    /// Licence string.
    pub license       : String,
    /// Author string.
    pub author        : String,
    /// Icon name or path.
    pub icon          : String,
    /// XKB layout the engine expects the client to be using. `default` means
    /// "whatever is there"; anything else is a layout the engine declares, and
    /// is why the frontend commits plain printable keys as text rather than
    /// replaying them (see `docs/multiplexer.md`).
    pub layout        : String,
    /// Selection rank; the daemon prefers higher.
    pub rank          : u32,
    /// Engine-specific hotkeys.
    pub hotkeys       : String,
    /// Short symbol for a status area, e.g. `あ`.
    pub symbol        : String,
    /// Setup command line, if the engine ships a configuration UI.
    pub setup         : String,
    /// XKB variant that goes with `layout`.
    pub layout_variant: String,
    /// XKB option that goes with `layout`.
    pub layout_option : String,
    /// Engine version.
    pub version       : String,
    /// gettext domain for translating `longname`/`description`.
    pub textdomain    : String,
    /// Property key whose icon overrides `icon` in the panel.
    pub icon_prop_key : String,
}

impl EngineDesc {
    /// Every field, one per line, for the devtest dump.
    ///
    /// Separate from [`std::fmt::Display`] because the one-line form is what
    /// tracing wants and the full dump is what "does our decoder agree with
    /// the daemon" wants.
    pub fn detail(&self) -> String {
        [
            format!("name           {}", self.name),
            format!("longname       {}", self.longname),
            format!("description    {}", self.description),
            format!("language       {}", self.language),
            format!("license        {}", self.license),
            format!("author         {}", self.author),
            format!("icon           {}", self.icon),
            format!("layout         {}", self.layout),
            format!("rank           {}", self.rank),
            format!("hotkeys        {}", self.hotkeys),
            format!("symbol         {}", self.symbol),
            format!("setup          {}", self.setup),
            format!("layout_variant {}", self.layout_variant),
            format!("layout_option  {}", self.layout_option),
            format!("version        {}", self.version),
            format!("textdomain     {}", self.textdomain),
            format!("icon_prop_key  {}", self.icon_prop_key),
        ]
        .join("\n")
    }
}

impl Serializable for EngineDesc {
    const TYPE_NAME: &'static str = "IBusEngineDesc";

    fn encode(&self, builder: StructureBuilder<'static>) -> Result<StructureBuilder<'static>> {
        Ok(builder
            .add_field(self.name.clone())
            .add_field(self.longname.clone())
            .add_field(self.description.clone())
            .add_field(self.language.clone())
            .add_field(self.license.clone())
            .add_field(self.author.clone())
            .add_field(self.icon.clone())
            .add_field(self.layout.clone())
            .add_field(self.rank)
            .add_field(self.hotkeys.clone())
            .add_field(self.symbol.clone())
            .add_field(self.setup.clone())
            .add_field(self.layout_variant.clone())
            .add_field(self.layout_option.clone())
            .add_field(self.version.clone())
            .add_field(self.textdomain.clone())
            .add_field(self.icon_prop_key.clone()))
    }

    fn decode(fields: &[Value<'_>]) -> Result<Self> {
        Ok(Self {
            name          : field_str(fields, 2, Self::TYPE_NAME)?,
            longname      : field_str(fields, 3, Self::TYPE_NAME)?,
            description   : field_str(fields, 4, Self::TYPE_NAME)?,
            language      : field_str(fields, 5, Self::TYPE_NAME)?,
            license       : field_str(fields, 6, Self::TYPE_NAME)?,
            author        : field_str(fields, 7, Self::TYPE_NAME)?,
            icon          : field_str(fields, 8, Self::TYPE_NAME)?,
            layout        : field_str(fields, 9, Self::TYPE_NAME)?,
            rank          : field_u32(fields, 10, Self::TYPE_NAME)?,
            hotkeys       : field_str(fields, 11, Self::TYPE_NAME)?,
            symbol        : field_str(fields, 12, Self::TYPE_NAME)?,
            setup         : field_str(fields, 13, Self::TYPE_NAME)?,
            layout_variant: field_str(fields, 14, Self::TYPE_NAME)?,
            layout_option : field_str(fields, 15, Self::TYPE_NAME)?,
            version       : field_str(fields, 16, Self::TYPE_NAME)?,
            textdomain    : field_str(fields, 17, Self::TYPE_NAME)?,
            icon_prop_key : field_str(fields, 18, Self::TYPE_NAME)?,
        })
    }
}

impl std::fmt::Display for EngineDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.longname)?;
        if !self.language.is_empty() {
            write!(f, " lang={}", self.language)?;
        }
        if !self.layout.is_empty() {
            write!(f, " layout={}", self.layout)?;
        }
        if !self.layout_variant.is_empty() {
            write!(f, "+{}", self.layout_variant)?;
        }
        if !self.layout_option.is_empty() {
            write!(f, "/{}", self.layout_option)?;
        }
        if !self.symbol.is_empty() {
            write!(f, " symbol={}", self.symbol)?;
        }

        write!(f, " rank={}", self.rank)
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::serialized::Context;
    use zbus::zvariant::{Endian, Str};

    /// Builds an `IBusSerializable` structure from already-typed fields.
    ///
    /// Lets a test hand-assemble the exact wire shape observed live, so that a
    /// decode test proves the decoder handles what the daemon actually sends
    /// rather than what our own encoder produces.
    fn raw(type_name: &str, fields: Vec<Value<'static>>) -> Value<'static> {
        let mut builder = StructureBuilder::new()
            .add_field(type_name.to_string())
            .add_field(HashMap::<String, Value<'static>>::new());
        for field in fields {
            builder = builder.append_field(field);
        }

        Value::Structure(builder.build().expect("hand-built structure"))
    }

    /// A `Value::Str` without the ceremony.
    fn s(text: &str) -> Value<'static> {
        Value::Str(Str::from(text.to_string()))
    }

    /// Encodes to real D-Bus bytes and reads them back.
    ///
    /// A `Value` -> `Value` round trip would prove nothing about the wire: the
    /// interesting failures (a variant nested one level too deep, an `av` of
    /// structures encoded as an array of structures) only appear once the
    /// signature is committed to bytes.
    fn roundtrip<T>(original: &T) -> T
    where
        T: Serializable + std::fmt::Debug + PartialEq,
    {
        let context = Context::new_dbus(Endian::Little, 0);
        let encoded = zbus::zvariant::to_bytes(context, &original.to_value().expect("encode"))
            .expect("serialise");
        let (value, _) = encoded.deserialize::<Value<'_>>().expect("deserialise");

        T::from_value(&value).expect("decode")
    }

    #[test]
    fn attribute_round_trips() {
        let original = Attribute {
            kind : ATTR_BACKGROUND,
            value: 0x00ff_ffff,
            start: 3,
            end  : 7,
        };
        assert_eq!(roundtrip(&original), original);
    }

    #[test]
    fn attr_list_round_trips() {
        let original = AttrList {
            attributes: vec![
                Attribute { kind: ATTR_UNDERLINE, value: 1, start: 0, end: 4 },
                Attribute { kind: ATTR_FOREGROUND, value: 0x0033_0033, start: 4, end: 6 },
            ],
        };
        assert_eq!(roundtrip(&original), original);
    }

    #[test]
    fn empty_attr_list_round_trips() {
        assert_eq!(roundtrip(&AttrList::default()), AttrList::default());
    }

    #[test]
    fn text_round_trips_with_attributes() {
        let original = Text {
            text : "にほんご".to_string(),
            attrs: AttrList {
                attributes: vec![Attribute {
                    kind : ATTR_UNDERLINE,
                    value: 1,
                    start: 0,
                    end  : 4,
                }],
            },
        };
        assert_eq!(roundtrip(&original), original);
    }

    #[test]
    fn plain_text_round_trips() {
        let original = Text::plain("hello");
        assert_eq!(roundtrip(&original), original);
        assert!(original.attrs.attributes.is_empty());
    }

    #[test]
    fn lookup_table_round_trips() {
        let original = LookupTable {
            page_size     : 9,
            cursor_pos    : 2,
            cursor_visible: true,
            round         : false,
            orientation   : ORIENTATION_VERTICAL,
            candidates    : vec![
                Text::plain("日本語"),
                Text::plain("にほんご"),
                Text::plain("ニホンゴ"),
            ],
            labels        : vec![Text::plain("1."), Text::plain("2."), Text::plain("3.")],
        };
        assert_eq!(roundtrip(&original), original);
    }

    #[test]
    fn engine_desc_round_trips() {
        let original = EngineDesc {
            name          : "mozc-jp".to_string(),
            longname      : "Mozc".to_string(),
            description   : "Japanese input method".to_string(),
            language      : "ja".to_string(),
            license       : "New BSD".to_string(),
            author        : "Google Inc.".to_string(),
            icon          : "/usr/share/ibus-mozc/product_icon.png".to_string(),
            layout        : "default".to_string(),
            rank          : 80,
            hotkeys       : String::new(),
            symbol        : "あ".to_string(),
            setup         : "/usr/lib/mozc/mozc_tool --mode=config_dialog".to_string(),
            layout_variant: String::new(),
            layout_option : String::new(),
            version       : "3.34".to_string(),
            textdomain    : "ibus-mozc".to_string(),
            icon_prop_key : "InputMode".to_string(),
        };
        assert_eq!(roundtrip(&original), original);
    }

    /// The exact `GlobalEngine` value this machine's daemon returns, keyed in
    /// by hand from the live introspection. If the field order ever drifts,
    /// this is the test that catches it — a round trip would not, because it
    /// would be wrong in both directions.
    #[test]
    fn decodes_the_live_global_engine_value() {
        let value = raw(
            "IBusEngineDesc",
            vec![
                s("xkb:us::eng"),
                s("English (US)"),
                s("English (US)"),
                s("en"),
                s("GPL"),
                s("Peng Huang <shawn.p.huang@gmail.com>"),
                s("ibus-keyboard"),
                s("us"),
                Value::U32(50),
                s(""),
                s(""),
                s(""),
                s(""),
                s(""),
                s(""),
                s(""),
                s(""),
            ],
        );

        let desc = EngineDesc::from_value(&value).expect("decode the live shape");
        assert_eq!(desc.name, "xkb:us::eng");
        assert_eq!(desc.longname, "English (US)");
        assert_eq!(desc.description, "English (US)");
        assert_eq!(desc.language, "en");
        assert_eq!(desc.license, "GPL");
        assert_eq!(desc.author, "Peng Huang <shawn.p.huang@gmail.com>");
        assert_eq!(desc.icon, "ibus-keyboard");
        assert_eq!(desc.layout, "us");
        assert_eq!(desc.rank, 50);
        assert_eq!(desc.hotkeys, "");
        assert_eq!(desc.icon_prop_key, "");
    }

    /// A payload whose type name is not the one the signature promised must be
    /// refused rather than decoded positionally into nonsense.
    #[test]
    fn rejects_the_wrong_type_name() {
        let value = raw("IBusLookupTable", vec![s("x"), Value::Value(Box::new(
            AttrList::default().to_value().expect("attr list"),
        ))]);
        assert!(Text::from_value(&value).is_err());
    }

    /// A truncated structure must be an error, not a panic: the daemon is a
    /// separate process and a version skew is a normal failure mode.
    #[test]
    fn rejects_a_short_structure() {
        assert!(EngineDesc::from_value(&raw("IBusEngineDesc", vec![s("mozc-jp")])).is_err());
    }
}
