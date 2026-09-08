use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Capabilities known to the core. DESIGN.md does not enumerate these; this
/// list grows with adapters. The aspect vocabulary of each is `VOCABULARY`
/// below; a test keeps the two in step.
pub const CAPABILITIES: &[&str] = &[
    "binary_sensor",
    "camera",
    "climate",
    "cover",
    "light",
    "lock",
    "person",
    "presence",
    "router",
    "sensor",
    "switch",
    "vpn",
];

/// One capability's aspect vocabulary: what an adapter binding it must
/// publish under which names, and what the family surfaces act on. This
/// is the public schema's side of "adapters speak homeostat vocabulary"
/// (docs/design.md, Dashboard): the base aspect is what commands target
/// and the dashboard widget renders; features are the optional aspects
/// an entity file may declare; notable names the reading that counts as
/// a deviation on `Now`. Rendered into docs/manifest.md.
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    pub name: &'static str,
    /// The aspect the capability's widget and cmd grant act on, if any.
    pub base: Option<&'static str>,
    /// Optional aspects an entity may declare in `features`, and other
    /// names the vocabulary reserves for this capability.
    pub aspects: &'static [&'static str],
    /// The reading and value that is out of the ordinary, if any.
    pub notable: Option<&'static str>,
    pub note: &'static str,
}

pub const VOCABULARY: &[Capability] = &[
    Capability {
        name: "binary_sensor",
        base: None,
        aspects: &[],
        notable: None,
        note: "A boolean under its native name.",
    },
    Capability {
        name: "camera",
        base: None,
        aspects: &["motion"],
        notable: None,
        note: "`motion` (bool). Media rides the go2rtc plane, never the bus.",
    },
    Capability {
        name: "climate",
        base: Some("setpoint"),
        aspects: &["indoor_temperature", "feed_temperature"],
        notable: None,
        note: "`setpoint` in °C is the family lever; the two readings are normalized when the device has them.",
    },
    Capability {
        name: "cover",
        base: None,
        aspects: &[],
        notable: None,
        note: "Reserved; no adapter binds it yet.",
    },
    Capability {
        name: "light",
        base: Some("on"),
        aspects: &["brightness", "color_temp"],
        notable: Some("on = true"),
        note: "`brightness` 0–254 (the Zigbee2MQTT scale the dashboard assumes), `color_temp` in mired.",
    },
    Capability {
        name: "lock",
        base: Some("locked"),
        aspects: &[],
        notable: Some("locked = false"),
        note: "",
    },
    Capability {
        name: "person",
        base: None,
        aspects: &["lat", "lon", "accuracy", "battery", "fixed_at"],
        notable: None,
        note: "Scalar position aspects; `fixed_at` is the fix's epoch timestamp. Room is always `person`.",
    },
    Capability {
        name: "presence",
        base: None,
        aspects: &["occupancy", "presence"],
        notable: None,
        note: "Either spelling is accepted; adapters pass their native one through.",
    },
    Capability {
        name: "router",
        base: None,
        aspects: &["wan"],
        notable: Some("wan = false"),
        note: "",
    },
    Capability {
        name: "sensor",
        base: None,
        aspects: &[],
        notable: None,
        note: "Numeric aspects under descriptive names (`temperature`, `humidity`); widgets come from what is published.",
    },
    Capability {
        name: "switch",
        base: Some("on"),
        aspects: &[],
        notable: None,
        note: "",
    },
    Capability {
        name: "vpn",
        base: None,
        aspects: &["up"],
        notable: Some("up = false"),
        note: "",
    },
];

/// Aspects every capability shares: `available` (device liveness, opt-in,
/// notable when false) and `{aspect}_valid` beside a reading the device
/// itself may stop trusting. Rendered with the vocabulary.
pub const COMMON_ASPECTS: &[(&str, &str)] = &[
    ("available", "bool, published on transition by the owning adapter when the protocol has a real loss signal; `false` is notable (docs/design.md, Availability)."),
    ("{aspect}_valid", "bool beside a reading the device itself may stop trusting; the value stands, the flag says stale."),
];

pub const SUPPORTED_SCHEMA: u32 = 1;

// The structs below ARE the manifest contract: `deny_unknown_fields` makes
// them complete, and `JsonSchema` makes them readable without the source —
// `homeostat schema`, the MCP `schema` tool, and the generated
// docs/manifest.md all derive from here (#4). Doc comments become field
// descriptions, so a rule worth knowing while authoring belongs on the
// field it constrains, and a rule the validator enforces beyond the shape
// names its error code.

/// A unit manifest: `units/<name>.toml`. One schema, three kinds (adapter,
/// automation, service); which sections a kind accepts is stated on each
/// section, and the validator refuses a mismatch with `invalid-manifest`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitManifest {
    /// Contract version. Must be 1 (`unsupported-schema` otherwise).
    pub schema: u32,
    pub unit: UnitSection,
    pub runtime: RuntimeSection,
    /// How an adapter reaches its backend. Required for adapters, allowed
    /// for services, refused for automations.
    pub discovery: Option<DiscoverySection>,
    /// The unit's declared bus surface. A unit gets exactly this and
    /// nothing else: the SDK refuses a publish outside it.
    pub bus: Option<BusSection>,
    /// Live parameters, keyed by name (`home/config/{unit}/{param}`).
    /// Names become key segments (`invalid-name`).
    pub params: Option<BTreeMap<String, ParamSpec>>,
    /// The entities this unit binds. Required for adapters, allowed for
    /// automations (virtual sensors), refused for services.
    pub entities: Option<EntitiesSection>,
    /// Human names for voice and the dashboard. Dashboard quality is a
    /// function of naming hygiene.
    pub naming: Option<UnitNaming>,
}

/// `[unit]`: identity.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitSection {
    /// Unique across the house; a bus key segment (`home/health/{unit}`,
    /// `home/config/{unit}/*`), so letters, digits, `_`, `-`, `.` only.
    /// `system` is reserved for the core.
    pub name: String,
    pub kind: UnitKind,
    pub description: Option<String>,
    /// Which repo files are this unit's inputs. Absent means `own` — the
    /// unit's command, its own entity files, its zone if it uses one.
    pub inputs: Option<UnitInputs>,
}

/// A unit whose model spans the WHOLE house (the dashboard) is changed by
/// any entity or manifest anywhere, not just by files it owns. Per-unit
/// change detection cannot infer that, so the unit declares it: otherwise
/// `apply` reports success while leaving the unit confidently stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UnitInputs {
    /// The unit's own files: its command, its entity files, its zone.
    Own,
    /// Every manifest and entity file in the house.
    House,
}

/// What a unit is, which decides the sections it may carry and how it is
/// ordered in an apply walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UnitKind {
    /// Puts devices on the bus: binds entities, needs `[discovery]` and
    /// `[entities]`, publishes their state, takes their commands.
    Adapter,
    /// Regulates: subscribes state, publishes commands at the automation
    /// band, and may bind read-only virtual entities via `[entities]`.
    Automation,
    /// Infrastructure with no entities (the recorder, the dashboard, the
    /// arbiter, the clock). May carry `[discovery]`.
    Service,
}

impl fmt::Display for UnitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnitKind::Adapter => "adapter",
            UnitKind::Automation => "automation",
            UnitKind::Service => "service",
        })
    }
}

/// `[runtime]`: how the supervisor runs the unit.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSection {
    /// Shell command, run from the house root with `HOMEOSTAT_UNIT` and
    /// `HOMEOSTAT_BUS` set. Typically `uv run units/<name>.py`.
    pub command: String,
    pub restart: RestartPolicy,
    /// Seconds between SIGTERM and SIGKILL at shutdown. Default 5.
    pub shutdown_grace_s: Option<u32>,
}

/// When the supervisor restarts an exited unit. Every restart carries
/// backoff and a circuit breaker; the policy only says which exits count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Restart after any exit, clean or not.
    Always,
    /// Restart only after a non-zero exit or a signal.
    OnFailure,
    /// Never restart; a stopped unit stays stopped until the next apply.
    Never,
}

/// `[discovery]`: how an adapter finds its backend. `static` needs
/// `endpoint`, `mdns` needs `service` (`invalid-manifest` otherwise).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoverySection {
    pub mode: DiscoveryMode,
    /// A URL such as `mqtt://host:1883`. `${VAR}` expands from the unit's
    /// environment at start; an unset variable is a startup error.
    pub endpoint: Option<String>,
    /// The mDNS service type to browse, e.g. `_esphomelib._tcp`.
    pub service: Option<String>,
}

/// How the backend address is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryMode {
    /// One configured `endpoint`.
    Static,
    /// Each device resolved individually by browsing `service`.
    Mdns,
}

/// `[bus]`: the unit's declared key surface. Keys are `home/{class}/...`
/// (`key-outside-schema` otherwise); a zone name in the room slot expands
/// to its rooms at plan time; `{room}`/`{entity}` templates expand per
/// bound entity and are valid only in adapters and automations
/// (`template-outside-binding-unit`).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BusSection {
    /// `[bus.subscribes]`: binding name → key expression. The SDK
    /// subscribes by binding name. A unit's own `home/config/{unit}/*` is
    /// implicit and never declared.
    #[serde(default)]
    pub subscribes: BTreeMap<String, String>,
    /// `[bus.publishes]`: binding name → what the unit may publish. This
    /// is what the plan resolves into the grant table.
    #[serde(default)]
    pub publishes: BTreeMap<String, PublishSpec>,
}

/// One publish the unit is allowed.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishSpec {
    /// Key expression. Under `home/state/` it must name a bound entity's
    /// room and entity literally or by template (`state-publish-unbound`).
    pub key: String,
    /// Required under `home/cmd/` (`publish-missing-capability`): the grant
    /// resolves onto bound entities of this capability that the key covers.
    /// Must be a known capability (`unknown-capability`).
    pub capability: Option<String>,
    /// The band commands leave at. Automations publish at `automation`;
    /// the family's surfaces (dashboard, voice) at `manual`, which always
    /// wins in arbitration.
    pub priority: Option<Priority>,
}

/// Command priority bands, lowest to highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Automation,
    Agent,
    Family,
    Manual,
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Priority::Automation => "automation",
            Priority::Agent => "agent",
            Priority::Family => "family",
            Priority::Manual => "manual",
        })
    }
}

/// `[params.<name>]`: one live parameter. The manifest default is the
/// value until something writes `home/config/{unit}/{param}`; a write
/// outside the constraint is refused with the old value still in force.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    #[serde(rename = "type")]
    pub param_type: ParamType,
    /// A TOML literal of the declared type (`invalid-default` otherwise):
    /// `true`, `30`, `0.5` (an integer literal is accepted for `float`),
    /// `"text"`, or `"22:00"` for `time`. Must satisfy the constraint.
    #[schemars(with = "serde_json::Value")]
    pub default: toml::Value,
    /// Inline table of constraint keys the type understands
    /// (`malformed-constraint` otherwise): `min`/`max` for `int` and
    /// `float`; `after`/`before` (`"HH:MM"`, may span midnight) for `time`.
    #[schemars(with = "Option<BTreeMap<String, serde_json::Value>>")]
    pub constraint: Option<BTreeMap<String, toml::Value>>,
    /// Who may change it live. `family` params appear as editable
    /// setpoints on the dashboard; anything else is visible there but
    /// written only through the repo (a propose) or the bus.
    pub editable_by: Option<EditableBy>,
}

/// Parameter value types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    Bool,
    Int,
    Float,
    String,
    /// A wall-clock time of day, `"HH:MM"`; the SDK serves it as a
    /// `datetime.time`.
    Time,
}

impl fmt::Display for ParamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParamType::Bool => "bool",
            ParamType::Int => "int",
            ParamType::Float => "float",
            ParamType::String => "string",
            ParamType::Time => "time",
        })
    }
}

/// Actor tiers that may write a parameter live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EditableBy {
    Owner,
    Family,
}

impl fmt::Display for EditableBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EditableBy::Owner => "owner",
            EditableBy::Family => "family",
        })
    }
}

/// `[entities]`: where this unit's entity files live.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntitiesSection {
    /// Directory relative to the house root, e.g. `entities/zigbee/`. Each
    /// `<name>.toml` in it is one bound entity; the stem is its name
    /// (`missing-entities-dir` if absent).
    pub dir: String,
}

/// `[naming]` on a unit.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitNaming {
    /// Swedish label.
    pub sv: Option<String>,
    /// English label; the dashboard falls back to the name with `_`
    /// replaced by spaces.
    pub en: Option<String>,
    /// Alternative spoken names.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Zone or room the unit belongs to, for grouping.
    pub room: Option<String>,
}

/// An entity file: `<entities dir>/<name>.toml`. The file stem is the
/// entity's name, unique across the house (`duplicate-entity-name`), and
/// a key segment (`invalid-name`).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityFile {
    /// Contract version. Must be 1.
    pub schema: u32,
    pub entity: EntitySection,
    pub naming: Option<EntityNaming>,
    pub write_policy: WritePolicy,
    /// `[inputs]`: device inputs fed from one source each, keyed by the
    /// adapter's own input name (e.g. `indoor_temperature_actual`). A fed
    /// input is a continuous signal with one master, not a command: it
    /// stops being a command aspect for this entity, never rides the
    /// arbiter, and staleness is the device's own validity window. Only a
    /// device entity can be fed (`virtual-entity-fed`); the adapter is the
    /// authority on which input names exist.
    pub inputs: Option<BTreeMap<String, InputSource>>,
}

/// The source of a fed input: an entity and one of its aspects, i.e. the
/// state key `home/state/{room}/{entity}/{aspect}`. The plan resolves it
/// and prints the edge, as it does a grant.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputSource {
    /// Name of the source entity; must exist (`input-unknown-entity`). Any
    /// owner will do — an automation's virtual sensor or another adapter's
    /// device.
    pub entity: String,
    /// The aspect to read. When the source is automation-owned, that
    /// automation's `[bus.publishes]` must cover the key
    /// (`input-unpublished-aspect`).
    pub aspect: String,
}

/// `[entity]`: what the device is and where.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntitySection {
    /// The adapter-native address (a zigbee2mqtt friendly name, an ESPHome
    /// node, a camera's go2rtc stream). Unique per adapter
    /// (`duplicate-entity-id`).
    pub id: String,
    /// One of: binary_sensor, camera, climate, cover, light, lock, person,
    /// presence, router, sensor, switch, vpn (`unknown-capability`). Decides
    /// the base aspect, the dashboard widget and which cmd grants apply.
    pub capability: String,
    /// Optional aspects beyond the capability's base, as the adapter
    /// names them (`brightness`, `color_temp` on a light). For a sensor it
    /// is descriptive only; its widgets come from the numeric aspects it
    /// publishes.
    #[serde(default)]
    pub features: Vec<String>,
    /// The single source of spatial truth for this entity. A key segment;
    /// not `home` or a key class (`reserved-room-name`). The pseudo-rooms
    /// `global` and `person` are for entities with no place.
    pub room: String,
}

/// `[naming]` on an entity.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityNaming {
    /// Swedish label.
    pub sv: Option<String>,
    /// English label; the dashboard falls back to the name with `_`
    /// replaced by spaces.
    pub en: Option<String>,
    /// Alternative spoken names.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// `[write_policy]`: who may command the entity and how conflicts resolve.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WritePolicy {
    pub mode: WriteMode,
    /// Exactly one unit binds each entity: an adapter, or an automation
    /// for virtual sensors. Must exist (`missing-owner-unit`) and be the
    /// unit whose entities dir holds this file (`owner-mismatch`).
    pub owner: String,
}

/// How commands toward the entity are governed. An automation-owned
/// (virtual) entity is read-only: `arbitrated` is refused
/// (`virtual-entity-arbitrated`) and no cmd grant may cover it
/// (`virtual-entity-commanded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WriteMode {
    /// Any granted writer may command it; last write wins.
    Shared,
    /// At most one automation-band writer may be granted
    /// (`exclusive-write-conflict`); manual-band surfaces sit above.
    Exclusive,
    /// Commands go through the arbiter (leases, bands, preemption); the
    /// house must bind an arbiter-class publish covering it
    /// (`arbitrated-uncovered`).
    Arbitrated,
}

impl fmt::Display for WriteMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WriteMode::Shared => "shared",
            WriteMode::Exclusive => "exclusive",
            WriteMode::Arbitrated => "arbitrated",
        })
    }
}

/// `zones.toml` at the house root: named sets of rooms that expand in the
/// room slot of key expressions.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ZonesFile {
    /// Contract version. Must be 1.
    pub schema: u32,
    /// `[zones]`: zone name → member rooms. A zone name is a key segment,
    /// not reserved (`reserved-zone-name`), not also a room
    /// (`zone-room-collision`); members must be rooms some entity binds
    /// (`zone-unknown-room`) and never pseudo-rooms (`zone-pseudo-room`).
    #[serde(default)]
    pub zones: BTreeMap<String, Vec<String>>,
}
