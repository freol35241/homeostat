//! The manifest contract as data: JSON Schema derived from the structs in
//! manifest.rs (which `deny_unknown_fields` makes complete), and a Markdown
//! rendering of it for docs/manifest.md. One source; a test pins the
//! checked-in document to the rendering so it cannot drift (#4).

use schemars::schema_for;
use serde_json::{json, Value};

/// The three files a house is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum File {
    Unit,
    Entity,
    Zones,
}

impl File {
    pub const ALL: [File; 3] = [File::Unit, File::Entity, File::Zones];

    pub fn parse(name: &str) -> Option<File> {
        match name {
            "unit" => Some(File::Unit),
            "entity" => Some(File::Entity),
            "zones" => Some(File::Zones),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            File::Unit => "unit",
            File::Entity => "entity",
            File::Zones => "zones",
        }
    }

    fn title(self) -> &'static str {
        match self {
            File::Unit => "Unit manifest (`units/<name>.toml`)",
            File::Entity => "Entity file (`<entities dir>/<name>.toml`)",
            File::Zones => "Zones (`zones.toml`)",
        }
    }
}

/// The JSON Schema for one file kind.
pub fn json(file: File) -> Value {
    let schema = match file {
        File::Unit => schema_for!(crate::manifest::UnitManifest),
        File::Entity => schema_for!(crate::manifest::EntityFile),
        File::Zones => schema_for!(crate::manifest::ZonesFile),
    };
    serde_json::to_value(schema).expect("schema serializes")
}

/// All three, keyed by file name.
pub fn all() -> Value {
    json!({
        "unit": json(File::Unit),
        "entity": json(File::Entity),
        "zones": json(File::Zones),
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
         JSON by `homeostat schema [unit|entity|zones]` and the MCP `schema` \
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

/// The capability vocabulary (manifest.rs, VOCABULARY): the base aspect,
/// the named optional aspects, and the notable reading per capability,
/// plus the aspects every capability shares. What docs/adapters.md points
/// at instead of carrying its own copy.
fn render_vocabulary(out: &mut String) {
    out.push_str("## Capability vocabulary\n\n");
    out.push_str(
        "What an entity of each capability publishes under which names \
         (docs/adapters.md, State). The base aspect is what commands target \
         and the dashboard widget renders; the other named aspects are what \
         `features` may declare or the vocabulary reserves; notable is the \
         reading that counts as a deviation on `Now`. Anything else an \
         adapter publishes passes through under its native name.\n\n",
    );
    out.push_str("| Capability | Base aspect | Other named aspects | Notable | Notes |\n|---|---|---|---|---|\n");
    for c in crate::manifest::VOCABULARY {
        let base = c
            .base
            .map(|b| format!("`{b}`"))
            .unwrap_or_else(|| "—".into());
        let aspects = if c.aspects.is_empty() {
            "—".to_string()
        } else {
            c.aspects
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let notable = c
            .notable
            .map(|n| format!("`{n}`"))
            .unwrap_or_else(|| "—".into());
        out.push_str(&format!(
            "| `{}` | {base} | {aspects} | {notable} | {} |\n",
            c.name,
            c.note.replace('|', "\\|")
        ));
    }
    out.push_str("\nEvery capability may also publish:\n\n");
    for (aspect, desc) in crate::manifest::COMMON_ASPECTS {
        out.push_str(&format!("- `{aspect}` — {desc}\n"));
    }
}

fn render_file(schema: &Value, out: &mut String) {
    let defs = schema.get("$defs").cloned().unwrap_or(json!({}));
    // Root first, then every definition in order of first reference — the
    // order a reader meets them in a file.
    let mut queue: Vec<(String, Value)> = vec![(String::new(), schema.clone())];
    let mut seen: Vec<String> = Vec::new();
    while !queue.is_empty() {
        let (name, node) = queue.remove(0);
        if !name.is_empty() {
            out.push_str(&format!("### {name}\n\n"));
        }
        if let Some(desc) = node.get("description").and_then(Value::as_str) {
            out.push_str(&format!("{}\n\n", desc));
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
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
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
        } else if let Some(values) = v.get("enum").and_then(Value::as_array) {
            out.extend(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|v| (v.to_string(), None)),
            );
        } else {
            return None;
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
            let (inner, refs) = spec
                .get("items")
                .map(type_name)
                .unwrap_or(("any".into(), vec![]));
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

    #[test]
    fn vocabulary_covers_every_capability_once() {
        use crate::manifest::{CAPABILITIES, VOCABULARY};
        let names: Vec<&str> = VOCABULARY.iter().map(|c| c.name).collect();
        assert_eq!(
            names, CAPABILITIES,
            "VOCABULARY rows must match CAPABILITIES, in order"
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
