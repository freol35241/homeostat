use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

/// Capabilities known to the core. DESIGN.md does not enumerate these; this
/// list grows with adapters.
pub const CAPABILITIES: &[&str] = &[
    "binary_sensor",
    "climate",
    "cover",
    "light",
    "lock",
    "presence",
    "sensor",
    "switch",
];

pub const SUPPORTED_SCHEMA: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitManifest {
    pub schema: u32,
    pub unit: UnitSection,
    pub runtime: RuntimeSection,
    pub discovery: Option<DiscoverySection>,
    pub bus: Option<BusSection>,
    pub params: Option<BTreeMap<String, ParamSpec>>,
    pub entities: Option<EntitiesSection>,
    pub naming: Option<UnitNaming>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitSection {
    pub name: String,
    pub kind: UnitKind,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnitKind {
    Adapter,
    Automation,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSection {
    pub command: String,
    pub restart: RestartPolicy,
    pub shutdown_grace_s: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverySection {
    pub mode: DiscoveryMode,
    pub endpoint: Option<String>,
    pub service: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryMode {
    Static,
    Mdns,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusSection {
    #[serde(default)]
    pub subscribes: BTreeMap<String, String>,
    #[serde(default)]
    pub publishes: BTreeMap<String, PublishSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishSpec {
    pub key: String,
    pub capability: Option<String>,
    pub priority: Option<Priority>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    #[serde(rename = "type")]
    pub param_type: ParamType,
    pub default: toml::Value,
    pub constraint: Option<BTreeMap<String, toml::Value>>,
    pub editable_by: Option<EditableBy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    Bool,
    Int,
    Float,
    String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitiesSection {
    pub dir: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitNaming {
    pub sv: Option<String>,
    pub en: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub room: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityFile {
    pub schema: u32,
    pub entity: EntitySection,
    pub naming: Option<EntityNaming>,
    pub write_policy: WritePolicy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySection {
    pub id: String,
    pub capability: String,
    #[serde(default)]
    pub features: Vec<String>,
    /// The single source of spatial truth for this entity.
    pub room: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityNaming {
    pub sv: Option<String>,
    pub en: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WritePolicy {
    pub mode: WriteMode,
    /// Exactly one adapter binds each entity.
    pub owner: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteMode {
    Shared,
    Exclusive,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZonesFile {
    pub schema: u32,
    #[serde(default)]
    pub zones: BTreeMap<String, Vec<String>>,
}
