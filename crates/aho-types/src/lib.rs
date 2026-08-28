//! Leaf DTO crate. Everything here is plain data shared by every other crate.
//! Rule (12): `aho-types` depends only on serde/serde_json/blake3; no runtime crate is reachable from it.

#![forbid(unsafe_code)]

use std::fmt;
use std::pin::Pin;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Boxed future used by dyn-compatible seam traits (02: "accept the boxing").
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// JSON value; the canonical data shape for args, results, config values, custom log bodies.
pub type Value = serde_json::Value;

/// JSON Schema for a tool input or a config key (schemars-compatible shape).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RootSchema(pub Value);

/// blake3 digest; serialized as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub const ZERO: Hash = Hash([0; 32]);
    pub fn of(bytes: &[u8]) -> Hash {
        Hash(*blake3::hash(bytes).as_bytes())
    }
    /// Hash of the canonical JSON encoding (object keys sorted by serde_json's BTreeMap order).
    pub fn of_json<T: Serialize + ?Sized>(v: &T) -> Hash {
        let bytes = serde_json::to_vec(v).unwrap_or_default();
        Hash::of(&bytes)
    }
    pub fn chain(prev: &Hash, bytes: &[u8]) -> Hash {
        let mut h = blake3::Hasher::new();
        h.update(&prev.0);
        h.update(bytes);
        Hash(*h.finalize().as_bytes())
    }
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn short(&self) -> String {
        self.to_hex()[..7].to_string()
    }
    pub fn from_hex(s: &str) -> Option<Hash> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Hash(out))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hash::from_hex(&s).ok_or_else(|| serde::de::Error::custom("bad hash hex"))
    }
}

macro_rules! id {
    ($(#[$m:meta])* $name:ident($inner:ty)) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub $inner);
    };
}

/// u128 ids serialize as decimal strings: serde's internally tagged enums (`Principal`,
/// `EntryKind`) buffer values through a representation with no u128 support, so a numeric u128
/// would round-trip on write and fail on read. Deserialization accepts strings or numbers.
macro_rules! wide_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub u128);

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl serde::de::Visitor<'_> for V {
                    type Value = u128;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a non-negative integer id as a decimal string or number")
                    }
                    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<u128, E> {
                        v.parse().map_err(E::custom)
                    }
                    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<u128, E> {
                        Ok(v as u128)
                    }
                    fn visit_u128<E: serde::de::Error>(self, v: u128) -> Result<u128, E> {
                        Ok(v)
                    }
                    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<u128, E> {
                        u128::try_from(v).map_err(E::custom)
                    }
                }
                d.deserialize_any(V).map($name)
            }
        }
    };
}

id!(
    /// Monotonic per log. `06`: reconnect replays from the last seen id.
    EntryId(u64)
);
wide_id!(SessionId);
wide_id!(AgentId);
id!(TurnId(u64));
id!(StepId(u64));
id!(Epoch(u32));
id!(
    /// Operation behind a tool call (16).
    OpId(u64)
);
id!(JobId(u64));
id!(LeaseId(u64));
id!(CheckpointId(u64));
id!(
    /// Milliseconds since the Unix epoch.
    Timestamp(u64)
);

impl Timestamp {
    pub fn now() -> Timestamp {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        Timestamp(ms)
    }
}

/// Tool call id as issued by the model.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(pub String);

/// `04`: who caused an entry. Mutating APIs take the typed principals in `aho-policy`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    Operator,
    Engineer,
    Agent { id: AgentId, turn: TurnId },
    System { name: String },
}

impl Principal {
    pub fn system(name: &str) -> Principal {
        Principal::System { name: name.to_string() }
    }
}

/// Closed enum over what a tool can do (02). Non-optional on every `ToolDecl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consequence {
    ReadOnly,
    WorkspaceWrite,
    /// Reachable by an agent only under `world = host` (07).
    HostWrite,
    NetworkEgress,
    SpawnsProcess,
    #[serde(rename = "controls_vm")]
    ControlsVM,
    ControlsDesktop,
    TalksToHuman,
}

impl Consequence {
    pub const ALL: [Consequence; 8] = [
        Consequence::ReadOnly,
        Consequence::WorkspaceWrite,
        Consequence::HostWrite,
        Consequence::NetworkEgress,
        Consequence::SpawnsProcess,
        Consequence::ControlsVM,
        Consequence::ControlsDesktop,
        Consequence::TalksToHuman,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Consequence::ReadOnly => "ReadOnly",
            Consequence::WorkspaceWrite => "WorkspaceWrite",
            Consequence::HostWrite => "HostWrite",
            Consequence::NetworkEgress => "NetworkEgress",
            Consequence::SpawnsProcess => "SpawnsProcess",
            Consequence::ControlsVM => "ControlsVM",
            Consequence::ControlsDesktop => "ControlsDesktop",
            Consequence::TalksToHuman => "TalksToHuman",
        }
    }
    pub fn parse(s: &str) -> Option<Consequence> {
        Consequence::ALL.into_iter().find(|c| c.name().eq_ignore_ascii_case(s))
    }
}

/// Bit set over `Consequence`.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ConsequenceSet(pub u16);

impl ConsequenceSet {
    pub const EMPTY: Self = Self(0);
    pub const ALL: Self = Self(0xff);
    pub const fn contains(&self, c: Consequence) -> bool {
        self.0 & (1 << c as u16) != 0
    }
    pub const fn with(self, c: Consequence) -> Self {
        Self(self.0 | (1 << c as u16))
    }
    pub const fn without(self, c: Consequence) -> Self {
        Self(self.0 & !(1 << c as u16))
    }
    /// Meet. The only combinator the policy lattice exposes to non-operators (04).
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn is_subset(&self, other: &Self) -> bool {
        self.0 & !other.0 == 0
    }
    pub fn iter(self) -> impl Iterator<Item = Consequence> {
        Consequence::ALL.into_iter().filter(move |c| self.contains(*c))
    }
    pub fn of(cs: &[Consequence]) -> Self {
        cs.iter().fold(Self::EMPTY, |s, c| s.with(*c))
    }
}

impl fmt::Debug for ConsequenceSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter().map(|c| c.name())).finish()
    }
}

impl fmt::Display for ConsequenceSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.iter().map(|c| c.name()).collect();
        f.write_str(&names.join(" · "))
    }
}

impl Serialize for ConsequenceSet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v: Vec<Consequence> = self.iter().collect();
        v.serialize(s)
    }
}

impl<'de> Deserialize<'de> for ConsequenceSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Vec::<Consequence>::deserialize(d)?;
        Ok(ConsequenceSet::of(&v))
    }
}

// ───────────────────────────── model-facing messages ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: Vec<u8>,
    },
    ToolUse {
        id: CallId,
        name: String,
        args: Value,
    },
    ToolResult {
        id: CallId,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    Thinking {
        text: String,
    },
    /// Opaque protocol state (signed reasoning/native history), retained for same-provider replay.
    /// Not user-visible text and never forwarded to a different protocol.
    ProviderMetadata {
        protocol: String,
        data: Value,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> ContentBlock {
        ContentBlock::Text { text: s.into() }
    }
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } | ContentBlock::Thinking { text } => Some(text),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Message {
        Message { role: Role::User, content: vec![ContentBlock::text(text)] }
    }
    pub fn assistant(content: Vec<ContentBlock>) -> Message {
        Message { role: Role::Assistant, content }
    }
    pub fn text(&self) -> String {
        self.content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("")
    }
}

/// What the model sees for a tool (02: derived from `ToolDecl`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: RootSchema,
    pub class: Consequence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl std::ops::Add for Usage {
    type Output = Usage;
    fn add(self, o: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + o.input_tokens,
            output_tokens: self.output_tokens + o.output_tokens,
            cache_read_tokens: self.cache_read_tokens + o.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens + o.cache_write_tokens,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Aborted,
    Error,
}

/// `10`: source labelling for injected context; UIs read metadata, never prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    Prompt,
    Snapshot,
    Catalog,
    Instructions,
    Notice,
}

/// A rendered UI card intent (10: `present_call` / `present_result` are pure).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "card", rename_all = "snake_case")]
pub enum Card {
    Generic { title: String, body: String },
    Terminal { command: String, output: String, exit: Option<i32> },
    Diff { path: String, hunks: Vec<String> },
    Search { query: String, hits: Vec<String> },
    Read { path: String, range: Option<(u32, u32)> },
    Web { url: String, title: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consequence_set_meet_never_grows() {
        let a = ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]);
        let b = ConsequenceSet::of(&[Consequence::WorkspaceWrite, Consequence::NetworkEgress]);
        let m = a.intersect(b);
        assert!(m.is_subset(&a) && m.is_subset(&b));
        assert_eq!(m, ConsequenceSet::of(&[Consequence::WorkspaceWrite]));
    }

    #[test]
    fn hash_hex_round_trips() {
        let h = Hash::of(b"aho");
        assert_eq!(Hash::from_hex(&h.to_hex()), Some(h));
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(serde_json::from_str::<Hash>(&json).unwrap(), h);
    }

    #[test]
    fn wide_ids_round_trip_inside_internally_tagged_enums() {
        let p = Principal::Agent { id: AgentId(u128::MAX - 7), turn: TurnId(3) };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(serde_json::from_value::<Principal>(v).unwrap(), p);
        // older numeric encodings still load
        let legacy = serde_json::json!({"kind": "agent", "id": 42, "turn": 1});
        assert_eq!(serde_json::from_value::<Principal>(legacy).unwrap(), Principal::Agent { id: AgentId(42), turn: TurnId(1) });
    }

    #[test]
    fn consequence_set_serializes_as_names() {
        let s = ConsequenceSet::of(&[Consequence::ControlsVM, Consequence::ReadOnly]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"["read_only","controls_vm"]"#);
        assert_eq!(serde_json::from_str::<ConsequenceSet>(&json).unwrap(), s);
    }
}
