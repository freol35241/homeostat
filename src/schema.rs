//! The manifest contract as data: JSON Schema derived from the structs in
//! manifest.rs, a Markdown rendering of it for docs/manifest.md, and the
//! vocabulary tables as a Python module for the SDK
//! (sdk/python/homeostat/vocabulary.py). Tests pin the checked-in files to
//! the renderings so they stay in step.

use schemars::schema_for;
use serde_json::{json, Value};

/// The four files a house is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum File {
    Unit,
    Entity,
    Zones,
    Dashboard,
}

impl File {
    /// Every file kind, in the order `homeostat schema` prints them.
    pub const ALL: [File; 4] = [File::Unit, File::Entity, File::Zones, File::Dashboard];

    /// The file kind a `homeostat schema` argument names.
    pub fn parse(name: &str) -> Option<File> {
        match name {
            "unit" => Some(File::Unit),
            "entity" => Some(File::Entity),
            "zones" => Some(File::Zones),
            "dashboard" => Some(File::Dashboard),
            _ => None,
        }
    }

    /// The name `homeostat schema` takes for this kind.
    pub fn name(self) -> &'static str {
        match self {
            File::Unit => "unit",
            File::Entity => "entity",
            File::Zones => "zones",
            File::Dashboard => "dashboard",
        }
    }

    fn title(self) -> &'static str {
        match self {
            File::Unit => "Unit manifest (`units/<name>.toml`)",
            File::Entity => "Entity file (`<entities dir>/<name>.toml`)",
            File::Zones => "Zones (`zones.toml`)",
            File::Dashboard => "Dashboard views (`dashboard.toml`)",
        }
    }
}

/// The JSON Schema for one file kind.
pub fn json(file: File) -> Value {
    let schema = match file {
        File::Unit => schema_for!(crate::manifest::UnitManifest),
        File::Entity => schema_for!(crate::manifest::EntityFile),
        File::Zones => schema_for!(crate::manifest::ZonesFile),
        File::Dashboard => schema_for!(crate::manifest::DashboardFile),
    };
    serde_json::to_value(schema).expect("schema serializes")
}

/// All four, keyed by file name.
pub fn all() -> Value {
    json!({
        "unit": json(File::Unit),
        "entity": json(File::Entity),
        "zones": json(File::Zones),
        "dashboard": json(File::Dashboard),
    })
}

/// docs/manifest.md: every section and field of every file, with types,
/// whether required, and the description from the struct's doc comment.
pub fn markdown() -> String {
    let mut out = String::new();
    out.push_str(
        "# Manifest reference\n\n\
         Generated from the manifest structs by `homeostat schema --markdown`; \
         do not edit (a test refuses a stale copy). The same schema is served as \
         JSON by `homeostat schema [unit|entity|zones|dashboard]` and the MCP `schema` \
         tool. Rules the validator enforces beyond the shape are named by their \
         error code; `homeostat explain <code>` (or the MCP `explain` tool) has \
         the paragraph for each. Reasoning lives in docs/design.md.\n\n",
    );
    for file in File::ALL {
        out.push_str(&format!("## {}\n\n", file.title()));
        render_file(&json(file), &mut out);
    }
    render_vocabulary(&mut out);
    out
}

/// The capability vocabulary (manifest.rs, VOCABULARY): the base aspect, the
/// named optional aspects, the commands with their value types, and the
/// notable reading per capability, plus the aspects every capability shares. docs/adapters.md points here instead of
/// carrying its own copy.
fn render_vocabulary(out: &mut String) {
    out.push_str("## Capability vocabulary\n\n");
    out.push_str(
        "What an entity of each capability publishes under which names \
         (docs/adapters.md, State). The base aspect is what commands target \
         and the dashboard widget renders; the other named aspects are what \
         `features` may declare or the vocabulary reserves; commands are the \
         aspects that take a command and the type of value each carries, a \
         feature only on an entity that declares it; notable is the \
         reading that counts as a deviation on `Now`; rest is the value the \
         device is safe at, which wins between two holds at one band. \
         Anything else an adapter publishes passes through under its native \
         name.\n\n",
    );
    out.push_str("| Capability | Base aspect | Other named aspects | Commands | Notable | Rest | Notes |\n|---|---|---|---|---|---|---|\n");
    for c in crate::manifest::VOCABULARY {
        let base = c.base.map_or_else(|| "—".into(), |b| format!("`{b}`"));
        let aspects = if c.aspects.is_empty() {
            "—".to_string()
        } else {
            c.aspects
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let commands = if c.commands.is_empty() {
            "—".to_string()
        } else {
            c.commands
                .iter()
                .map(|(a, ty)| format!("`{a}` ({ty})"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let notable = c.notable.map_or_else(|| "—".into(), |n| format!("`{n}`"));
        let rest = c
            .rest
            .map_or_else(|| "—".into(), |(a, v)| format!("`{a} = {v}`"));
        out.push_str(&format!(
            "| `{}` | {base} | {aspects} | {commands} | {notable} | {rest} | {} |\n",
            c.name,
            c.note.replace('|', "\\|")
        ));
    }
    out.push_str("\nEvery capability may also publish:\n\n");
    for (aspect, desc) in crate::manifest::COMMON_ASPECTS {
        out.push_str(&format!("- `{aspect}` — {desc}\n"));
    }
    out.push_str("\n### Command options\n\n");
    out.push_str(
        "What a command may carry beside its value, in the envelope's \
         `options` (docs/design.md#command-options). An option changes how \
         the device reaches the value, never the value, so an adapter that \
         cannot honour one ignores it.\n\n",
    );
    out.push_str("| Capability | Option | Aspects | Notes |\n|---|---|---|---|\n");
    for c in crate::manifest::VOCABULARY {
        for o in c.options {
            let aspects = o
                .aspects
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "| `{}` | `{}` | {aspects} | {} |\n",
                c.name,
                o.name,
                o.note.replace('|', "\\|")
            ));
        }
    }
}

/// sdk/python/homeostat/vocabulary.py: the core's tables that the SDK and
/// the adapters read, as Python literals. Only tables a Python unit reads
/// are rendered. Rows follow the Rust tables' order, which a test keeps in
/// name order for VOCABULARY.
pub fn sdk_vocabulary() -> String {
    use crate::manifest::{
        implicit_enabled, Priority, BRIGHTNESS_RANGE, ENABLED_PARAM, HOLD_WORDS, VOCABULARY,
    };
    let mut out = String::from(
        "\"\"\"The core's vocabulary tables, as the SDK and the adapters read them.\n\
         \n\
         Generated from src/manifest.rs and src/keyspace.rs by `sdk_vocabulary` in\n\
         src/schema.rs; do not edit (a test refuses a stale copy). Regenerate with\n\
         `HOMEOSTAT_BLESS=1 cargo test --lib sdk_vocabulary`. docs/manifest.md\n\
         renders the same vocabulary for people.\n\
         \"\"\"\n\n",
    );
    let priorities: Vec<String> = Priority::ALL.iter().map(Priority::to_string).collect();
    out.push_str("# Command priority bands, lowest to highest (`Priority::ALL`).\n");
    out.push_str(&format!(
        "CMD_PRIORITIES = {}\n\n",
        python_tuple(priorities.iter().map(String::as_str))
    ));
    out.push_str(
        "# The words a manual-band command's `hold` may carry besides an instant or\n\
         # \"HH:MM\" (`HOLD_WORDS`).\n",
    );
    out.push_str(&format!(
        "HOLD_WORDS = {}\n\n",
        python_tuple(HOLD_WORDS.iter().copied())
    ));
    out.push_str(
        "# The classes addressed per entity, as home/{class}/{room}/{entity}/{aspect}\n\
         # (`ENTITY_ADDRESSED` in src/keyspace.rs).\n",
    );
    out.push_str(&format!(
        "ENTITY_ADDRESSED = {}\n\n",
        python_tuple(crate::keyspace::ENTITY_ADDRESSED.iter().copied())
    ));
    out.push_str("# The aspect each capability's commands target by default (`base`).\n");
    python_dict(
        &mut out,
        "BASE_ASPECTS",
        VOCABULARY
            .iter()
            .filter_map(|c| Some((c.name, python_str(c.base?)))),
    );
    out.push_str(
        "# The aspects that take a command, per capability, with the type of value\n\
         # each carries (`commands`, `ValueType`). A feature takes commands only on\n\
         # an entity that declares it.\n",
    );
    python_dict(
        &mut out,
        "COMMANDS",
        VOCABULARY
            .iter()
            .filter(|c| !c.commands.is_empty())
            .map(|c| {
                let commands: Vec<String> = c
                    .commands
                    .iter()
                    .map(|(aspect, ty)| {
                        format!("{}: {}", python_str(aspect), python_str(&ty.to_string()))
                    })
                    .collect();
                (c.name, format!("{{{}}}", commands.join(", ")))
            }),
    );
    out.push_str("# The aspect and value each capability's device is safe at (`rest`).\n");
    python_dict(
        &mut out,
        "REST_VALUES",
        VOCABULARY.iter().filter_map(|c| {
            let (aspect, value) = c.rest?;
            Some((
                c.name,
                format!("({}, {})", python_str(aspect), python_literal(value)),
            ))
        }),
    );
    out.push_str(
        "# The command options per capability, with the aspects whose commands take\n\
         # each (`options`).\n",
    );
    python_dict(
        &mut out,
        "OPTIONS",
        VOCABULARY
            .iter()
            .filter(|c| !c.options.is_empty())
            .map(|c| {
                let options: Vec<String> = c
                    .options
                    .iter()
                    .map(|o| {
                        format!(
                            "{}: {}",
                            python_str(o.name),
                            python_tuple(o.aspects.iter().copied())
                        )
                    })
                    .collect();
                (c.name, format!("{{{}}}", options.join(", ")))
            }),
    );
    out.push_str(
        "# The aspects a room sensor publishes, whichever its device observes\n\
         # (the `occupancy` row's `aspects`).\n",
    );
    let occupancy = crate::manifest::capability("occupancy").expect("occupancy is in VOCABULARY");
    out.push_str(&format!(
        "OCCUPANCY_ASPECTS = {}\n\n",
        python_tuple(occupancy.aspects.iter().copied())
    ));
    let (lowest, highest) = BRIGHTNESS_RANGE;
    out.push_str("# A light's lowest and highest `brightness` (`BRIGHTNESS_RANGE`).\n");
    out.push_str(&format!("BRIGHTNESS_RANGE = ({lowest}, {highest})\n\n"));
    // Destructured, so a new field of ParamSpec is a compile error here.
    let crate::manifest::ParamSpec {
        param_type,
        default,
        constraint,
        editable_by,
    } = implicit_enabled();
    assert!(
        constraint.is_none(),
        "implicit_enabled has a constraint: render it here"
    );
    let mut spec = vec![
        format!("\"type\": {}", python_str(&param_type.to_string())),
        format!("\"default\": {}", python_literal(&default.to_string())),
    ];
    if let Some(editable_by) = editable_by {
        spec.push(format!(
            "\"editable_by\": {}",
            python_str(&editable_by.to_string())
        ));
    }
    out.push_str(
        "# The parameter that pauses an automation, and its spec when the manifest\n\
         # does not declare it (`ENABLED_PARAM`, `implicit_enabled`).\n",
    );
    out.push_str(&format!("ENABLED_PARAM = {}\n", python_str(ENABLED_PARAM)));
    out.push_str(&format!("IMPLICIT_ENABLED = {{{}}}\n", spec.join(", ")));
    out
}

/// `name = {...}` with one `"key": value,` line per row.
fn python_dict<'a>(out: &mut String, name: &str, rows: impl Iterator<Item = (&'a str, String)>) {
    out.push_str(&format!("{name} = {{\n"));
    for (key, value) in rows {
        out.push_str(&format!("    {}: {value},\n", python_str(key)));
    }
    out.push_str("}\n\n");
}

/// A tuple of strings, with the trailing comma a one-element tuple needs.
fn python_tuple<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<String> = items.map(python_str).collect();
    match items.len() {
        1 => format!("({},)", items[0]),
        _ => format!("({})", items.join(", ")),
    }
}

/// A string as a Python literal. A JSON string is one: its escapes are
/// valid Python escapes.
fn python_str(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

/// A value as the vocabulary writes it (`false`, `30`), spelled as Python
/// spells it. Anything but a boolean, null or a number panics, so a new
/// kind of value is rendered on purpose.
fn python_literal(literal: &str) -> &str {
    match literal {
        "true" => "True",
        "false" => "False",
        "null" => "None",
        number if number.parse::<f64>().is_ok_and(f64::is_finite) => number,
        other => panic!("no Python spelling for the vocabulary value {other}"),
    }
}

fn render_file(schema: &Value, out: &mut String) {
    let defs = schema.get("$defs").cloned().unwrap_or(json!({}));
    // Root first, then every definition in order of first reference, which is
    // the order a reader meets them in a file.
    let mut queue: Vec<(String, Value)> = vec![(String::new(), schema.clone())];
    let mut seen: Vec<String> = Vec::new();
    while !queue.is_empty() {
        let (name, node) = queue.remove(0);
        if !name.is_empty() {
            out.push_str(&format!("### {name}\n\n"));
        }
        if let Some(desc) = node.get("description").and_then(Value::as_str) {
            out.push_str(&format!("{desc}\n\n"));
        }
        if let Some(values) = enum_values(&node) {
            for (value, desc) in values {
                match desc {
                    Some(d) => out.push_str(&format!("- `{value}` — {d}\n")),
                    None => out.push_str(&format!("- `{value}`\n")),
                }
            }
            out.push('\n');
            continue;
        }
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map_or_default(|a| a.iter().filter_map(Value::as_str).collect());
        if let Some(props) = node.get("properties").and_then(Value::as_object) {
            out.push_str("| Field | Type | Required | Description |\n|---|---|---|---|\n");
            for (field, spec) in props {
                let (ty, refs) = type_name(spec);
                for r in refs {
                    if !seen.contains(&r) {
                        seen.push(r.clone());
                        if let Some(def) = defs.get(&r) {
                            queue.push((r, def.clone()));
                        }
                    }
                }
                let req = if required.contains(&field.as_str()) {
                    "yes"
                } else {
                    "no"
                };
                let desc = spec
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .replace('\n', " ")
                    .replace('|', "\\|");
                out.push_str(&format!("| `{field}` | {ty} | {req} | {desc} |\n"));
            }
            out.push('\n');
        }
    }
}

/// For an enum schema: its values with per-variant descriptions.
fn enum_values(node: &Value) -> Option<Vec<(String, Option<String>)>> {
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        return Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|v| (v.to_string(), None))
                .collect(),
        );
    }
    // Variants with a description come out one per `const`; undocumented
    // neighbours are grouped into one `enum` entry.
    let variants = node.get("oneOf").and_then(Value::as_array)?;
    let mut out = Vec::new();
    for v in variants {
        if let Some(value) = v.get("const").and_then(Value::as_str) {
            let desc = v
                .get("description")
                .and_then(Value::as_str)
                .map(|d| d.replace('\n', " "));
            out.push((value.to_string(), desc));
        } else {
            let values = v.get("enum").and_then(Value::as_array)?;
            out.extend(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|v| (v.to_string(), None)),
            );
        }
    }
    Some(out)
}

/// A readable type for one field schema, plus the definitions it refers to.
fn type_name(spec: &Value) -> (String, Vec<String>) {
    if let Some(r) = spec.get("$ref").and_then(Value::as_str) {
        let name = r.rsplit('/').next().unwrap_or(r).to_string();
        return (format!("[{name}](#{})", name.to_lowercase()), vec![name]);
    }
    if let Some(any) = spec.get("anyOf").and_then(Value::as_array) {
        let inner: Vec<&Value> = any
            .iter()
            .filter(|s| s.get("type") != Some(&json!("null")))
            .collect();
        if inner.len() == 1 {
            return type_name(inner[0]);
        }
    }
    if let Some(values) = spec.get("enum").and_then(Value::as_array) {
        let list: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(|v| format!("`{v}`"))
            .collect();
        return (list.join(" \\| "), vec![]);
    }
    let ty = match spec.get("type") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("any")
            .to_string(),
        _ => "any".to_string(),
    };
    match ty.as_str() {
        "array" => {
            let (inner, refs) = spec.get("items").map_or(("any".into(), vec![]), type_name);
            (format!("list of {inner}"), refs)
        }
        "object" => match spec.get("additionalProperties") {
            Some(ap) if ap.is_object() => {
                let (inner, refs) = type_name(ap);
                (format!("table of name → {inner}"), refs)
            }
            _ => ("table".to_string(), vec![]),
        },
        other => (other.to_string(), vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_closed_and_cover_the_toml_valued_fields() {
        for file in File::ALL {
            let schema = json(file);
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{}: deny_unknown_fields must reach the schema",
                file.name()
            );
        }
        let unit = json(File::Unit);
        let param = &unit["$defs"]["ParamSpec"]["properties"];
        assert!(param.get("default").is_some(), "{unit}");
        assert!(param.get("constraint").is_some(), "{unit}");
        assert!(param["type"]["$ref"].is_string(), "{param}");
    }

    /// `manifest::capability` returns the first row of a name, so a second
    /// row would never be read.
    #[test]
    fn vocabulary_names_each_capability_once() {
        let names: Vec<&str> = crate::manifest::VOCABULARY.iter().map(|c| c.name).collect();
        assert!(
            names.windows(2).all(|w| w[0] < w[1]),
            "VOCABULARY rows must be in name order, one per capability: {names:?}"
        );
    }

    /// The base aspect is what commands target and an option rides a
    /// command, so both take one. A command goes to an aspect the
    /// capability has.
    #[test]
    fn commands_cover_the_base_and_every_option_aspect() {
        for c in crate::manifest::VOCABULARY {
            let commanded = |aspect: &str| c.commands.iter().any(|(a, _)| *a == aspect);
            if let Some(base) = c.base {
                assert!(commanded(base), "{}: base {base} takes no command", c.name);
            }
            for o in c.options {
                for aspect in o.aspects {
                    assert!(
                        commanded(aspect),
                        "{}: option {} rides {aspect}, which takes no command",
                        c.name,
                        o.name
                    );
                }
            }
            for (aspect, _) in c.commands {
                assert!(
                    c.base == Some(*aspect) || c.aspects.contains(aspect),
                    "{}: command {aspect} is neither the base nor a named aspect",
                    c.name
                );
            }
        }
    }

    /// docs/manifest.md gives a light's brightness scale in the light row's
    /// note, which is prose, so this keeps it to `BRIGHTNESS_RANGE`.
    #[test]
    fn light_note_states_the_brightness_range() {
        let (lowest, highest) = crate::manifest::BRIGHTNESS_RANGE;
        let light = crate::manifest::capability("light").expect("light is in VOCABULARY");
        assert!(
            light
                .note
                .contains(&format!("`brightness` {lowest}–{highest} ")),
            "the light note does not state BRIGHTNESS_RANGE"
        );
    }

    /// The dashboard reads every name in `OCCUPANCY_ASPECTS` as a presence
    /// reading (`occupancyReading` in dashboard/logic/index.js), so a name
    /// added to the occupancy row is a change to the page too.
    #[test]
    fn occupancy_aspects_are_the_presence_readings() {
        let occupancy =
            crate::manifest::capability("occupancy").expect("occupancy is in VOCABULARY");
        assert_eq!(occupancy.aspects, ["motion", "occupied"]);
    }

    #[test]
    fn command_options_name_aspects_their_capability_has() {
        for c in crate::manifest::VOCABULARY {
            for o in c.options {
                for aspect in o.aspects {
                    assert!(
                        c.base == Some(*aspect) || c.aspects.contains(aspect),
                        "{}: option {} names aspect {aspect}, which the capability does not",
                        c.name,
                        o.name
                    );
                }
            }
        }
    }

    /// docs/widgets.md is the dashboard's vocabulary with pictures
    /// (`scripts/widget_gallery.py`). The pixels differ by renderer, so they
    /// are not pinned. Its sections are. A widget kind the parser accepts must
    /// have a section in the gallery, or nobody can look it up.
    #[test]
    fn docs_widgets_md_shows_every_widget_kind() {
        let gallery = include_str!("../docs/widgets.md");
        let schema = json(File::Dashboard);
        let kinds = enum_values(&schema["$defs"]["WidgetKind"]).expect("WidgetKind is an enum");
        assert!(!kinds.is_empty());
        for (kind, _) in kinds {
            assert!(
                gallery.contains(&format!("\n## `{kind}`\n")),
                "docs/widgets.md has no section for `{kind}`: run \
                 `uv run --script scripts/widget_gallery.py`"
            );
        }
    }

    /// `Priority::ALL` names every band the schema knows, once, so the
    /// SDK's ranking misses none.
    #[test]
    fn priority_all_names_every_band() {
        use crate::manifest::Priority;
        let schema = serde_json::to_value(schema_for!(Priority)).expect("serializes");
        let mut known: Vec<String> = enum_values(&schema)
            .expect("Priority is an enum")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let mut all: Vec<String> = Priority::ALL.iter().map(Priority::to_string).collect();
        known.sort();
        all.sort();
        assert_eq!(all, known);
    }

    /// The SDK's vocabulary module is the rendering. With
    /// `HOMEOSTAT_BLESS=1` the test first writes the rendering to the file,
    /// after a change to the tables; review the diff.
    #[test]
    fn sdk_vocabulary_py_is_current() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/sdk/python/homeostat/vocabulary.py"
        );
        let rendered = sdk_vocabulary();
        if std::env::var("HOMEOSTAT_BLESS").as_deref() == Ok("1") {
            std::fs::write(path, &rendered).expect("vocabulary.py is writable");
        }
        let checked_in = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            checked_in == rendered,
            "sdk/python/homeostat/vocabulary.py is stale: run \
             `HOMEOSTAT_BLESS=1 cargo test --lib sdk_vocabulary`"
        );
    }

    #[test]
    fn docs_manifest_md_is_current() {
        let checked_in = include_str!("../docs/manifest.md");
        assert!(
            checked_in == markdown(),
            "docs/manifest.md is stale: run `cargo run -- schema --markdown > docs/manifest.md`"
        );
    }
}
