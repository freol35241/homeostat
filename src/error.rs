use std::fmt;

/// A single validation failure with a stable, machine-comparable rendering:
/// `error[<code>] <subject>: <message> (<file>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub code: &'static str,
    pub subject: String,
    pub message: String,
    /// House-relative path, when the error is attributable to one file.
    pub file: Option<String>,
}

impl ValidationError {
    pub fn new(
        code: &'static str,
        subject: impl Into<String>,
        message: impl Into<String>,
        file: Option<String>,
    ) -> Self {
        Self {
            code,
            subject: subject.into(),
            message: message.into(),
            file,
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}] {}: {}", self.code, self.subject, self.message)?;
        if let Some(file) = &self.file {
            write!(f, " ({file})")?;
        }
        Ok(())
    }
}

/// Deterministic rendering used by the CLI and the corpus tests.
pub fn render_sorted(errors: &[ValidationError]) -> Vec<String> {
    let mut lines: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
    lines.sort();
    lines.dedup();
    lines
}

/// Every error code the pipeline can emit, with the paragraph a reader (or
/// an agent) needs to fix it: what the rule is and why it exists. The
/// only registry — `homeostat explain`, the MCP `explain` tool, and the
/// explanations appended to refused plans all read from here, and a test
/// asserts every code emitted in the source has an entry and vice versa.
pub const CODES: &[(&str, &str)] = &[
    (
        "parse-error",
        "A file under units/, an entities dir, or zones.toml could not be read \
         or is not valid TOML; the subject is the file and the message is the \
         parser's. Nothing else in that file is checked until it parses.",
    ),
    (
        "unsupported-schema",
        "Every manifest, entity file and zones.toml begins with `schema = N`. \
         This core supports schema 1 only; a file from a newer or older \
         contract is refused rather than half-read.",
    ),
    (
        "missing-units-dir",
        "A house repo is a directory with a `units/` subdirectory holding one \
         TOML manifest per unit. Without it the path is not a house.",
    ),
    (
        "missing-entities-dir",
        "An adapter's or automation's `[entities] dir` names a directory, \
         relative to the house root, that does not exist. Each entity the \
         unit binds is one `<name>.toml` file in it; the file stem is the \
         entity's name.",
    ),
    (
        "invalid-name",
        "Unit, parameter, entity, room and zone names become bus key segments \
         (`home/state/{room}/{entity}/...`, `home/config/{unit}/{param}`), so \
         each must be non-empty ASCII letters, digits, `_`, `-` or `.`.",
    ),
    (
        "reserved-unit-name",
        "`system` is the core's own unit name: it serves `home/meta/system/**` \
         itself, so no manifest may claim it.",
    ),
    (
        "reserved-room-name",
        "A room may not be `home` or a key class (state, cmd, arbiter, config, \
         meta, health, clock, history, discovery), since rooms sit in the key \
         path right after the class. The pseudo-rooms `global` and `person` are \
         the exception: an entity with no place in the house lives in one of \
         those.",
    ),
    (
        "reserved-zone-name",
        "A zone name may not be `home`, a key class, or a pseudo-room \
         (`global`, `person`). Zones expand in the room slot of key \
         expressions, and pseudo-rooms are not places, so here there is no \
         exception.",
    ),
    (
        "zone-room-collision",
        "A zone and a room share a name. A zone reference in a key expression \
         expands to its member rooms, so a name cannot mean both a place and \
         a set of places.",
    ),
    (
        "zone-pseudo-room",
        "A zone lists `global` or `person` as a member. Pseudo-rooms hold \
         entities that have no place; a zone is a set of places.",
    ),
    (
        "zone-unknown-room",
        "A zone lists a room no entity is bound to. Rooms exist by being named \
         in entity files; zones.toml groups them and cannot invent one.",
    ),
    (
        "duplicate-unit-name",
        "Two manifests declare the same `[unit] name`. The name is the unit's \
         identity on the bus (`home/health/{unit}`, `home/config/{unit}/*`) and \
         in the supervisor, so it must be unique across the house.",
    ),
    (
        "duplicate-entity-name",
        "Two entity files, possibly under different adapters, share a file \
         stem. Entity names are house-global (`home/state/{room}/{entity}/...` \
         is addressed by name, not by owner), so the stem must be unique.",
    ),
    (
        "duplicate-entity-id",
        "Two entity files bound to the same adapter share an `id`. The id is the \
         adapter-native address (a zigbee2mqtt topic segment, an ESPHome node), \
         and one device cannot be two entities of one adapter.",
    ),
    (
        "invalid-manifest",
        "The manifest parsed, but its sections do not fit its `kind`. Adapters \
         require `[entities]` and `[discovery]`; `[entities]` is valid only for \
         adapters and automations; `[discovery]` only for adapters and \
         services; discovery mode `static` needs `endpoint`, mode `mdns` needs \
         `service`.",
    ),
    (
        "unknown-capability",
        "An entity file's `capability`, or a cmd publish's `capability`, is not \
         one the core knows. The vocabulary is fixed (binary_sensor, camera, \
         climate, cover, light, lock, person, presence, router, sensor, switch, \
         vpn) because grants, the arbiter and the dashboard key on it.",
    ),
    (
        "missing-owner-unit",
        "An entity file's `[write_policy] owner` names a unit that does not \
         exist in units/.",
    ),
    (
        "owner-mismatch",
        "An entity file's `[write_policy] owner` names a unit other than the one \
         whose `[entities] dir` the file sits in. The binding unit is the owner \
         by construction; the field must agree with the file's location.",
    ),
    (
        "virtual-entity-arbitrated",
        "An entity bound by an automation is virtual. If it takes commands it \
         is a latch: a command sets its state and last write wins, with no \
         device to contend for and no hold to expire, so arbitration has \
         nothing to order. A physical button's press travels at the \
         automation band yet is family intent, and arbitration would rank \
         it below the dashboard. Use `shared` or `exclusive`.",
    ),
    (
        "virtual-entity-commanded",
        "A cmd-class publish grant resolves onto an entity bound by an \
         automation that does not subscribe to that entity's cmd keys \
         (`home/cmd/{room}/{entity}/**`), so the command would reach nobody. \
         Either the owner is a read-only virtual sensor — narrow the publish \
         key or its capability — or it is meant to be a latch and needs the \
         subscription in its `[bus.subscribes]`.",
    ),
    (
        "grant-cycle",
        "Automations that bind commandable virtual entities are walk-order \
         edge sources like adapters: an owner starts before the units \
         commanding its entities. These units each command an entity another \
         of them binds, so no order exists. A latch must not command its own \
         commanders; break the loop by making one side a state subscription.",
    ),
    (
        "invalid-default",
        "A parameter's `default` does not fit its declared `type`, or violates \
         the parameter's own constraint. Types: bool, int, float (an integer \
         literal is accepted), string, time (an ISO string such as \
         \"22:00\").",
    ),
    (
        "malformed-constraint",
        "A parameter's `constraint` uses a key the type does not understand \
         (min/max on int and float, after/before on time), gives it a value \
         of the wrong type, or has min greater than max.",
    ),
    (
        "key-outside-schema",
        "A `[bus]` key expression must be `home/{class}/...`: it starts with \
         `home/`, names a known class literally (state, cmd, arbiter, config, \
         meta, health, clock, history, discovery), and has at least one \
         segment after the class.",
    ),
    (
        "template-outside-binding-unit",
        "A `[bus]` key uses `{room}` or `{entity}` templates, which expand per \
         bound entity and so are valid only in units that bind entities: \
         adapters and automations. A service names concrete keys or \
         wildcards.",
    ),
    (
        "publish-missing-capability",
        "A publish under `home/cmd/` must declare `capability`. The grant table \
         resolves a cmd publish onto the entities of that capability its key \
         covers, and a plan prints the result; without the capability there \
         is nothing to resolve.",
    ),
    (
        "exclusive-write-conflict",
        "An entity with `write_policy.mode = \"exclusive\"` is covered by more \
         than one automation-band cmd grant. Exclusivity constrains the \
         automation band only: manual-band units (the dashboard, voice) sit \
         above it by construction and do not count.",
    ),
    (
        "arbitrated-uncovered",
        "An entity with `write_policy.mode = \"arbitrated\"` has no unit \
         publishing under `home/arbiter/` whose key covers it. Arbitration is \
         a unit (adapters/arbiter.py), not core machinery: the house must \
         bind one before the first arbitrated entity plans.",
    ),
    (
        "virtual-entity-fed",
        "An `[inputs]` block sits on an entity bound by an automation. A fed \
         input is a device's control input (docs/design.md, Device feeds); a \
         virtual entity has no device behind it and nothing to feed.",
    ),
    (
        "input-unknown-entity",
        "An `[inputs]` entry names a source entity that no unit binds. The \
         source is referenced by entity name and aspect — the identity the \
         bus keys derive from — so the entity must exist in the house.",
    ),
    (
        "input-unpublished-aspect",
        "An `[inputs]` entry reads an aspect of an automation-owned entity \
         that the automation's `[bus.publishes]` does not cover. Virtual \
         sensors name their aspects literally in the publish key, so a feed \
         from one is checked at plan time; nothing would ever arrive on the \
         key otherwise.",
    ),
    (
        "state-publish-unbound",
        "A publish under `home/state/` must name, literally, the room and entity \
         of an entity this unit binds (or use `{room}`/`{entity}` templates). \
         State keys belong to the binding unit; an automation that wants to \
         publish a derived value needs an entity file for it, which also puts \
         the value in front of the recorder and the dashboard.",
    ),
];

/// The explanation registered for `code`, if any.
pub fn explain(code: &str) -> Option<&'static str> {
    CODES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, text)| *text)
}

/// One `code: explanation` paragraph per distinct code in `errors`, in code
/// order — appended wherever a refused plan is reported so the reader has
/// the rule next to the failure.
pub fn explanations(errors: &[ValidationError]) -> Vec<String> {
    let mut codes: Vec<&str> = errors.iter().map(|e| e.code).collect();
    codes.sort();
    codes.dedup();
    codes
        .into_iter()
        .map(|code| {
            format!(
                "{code}: {}",
                explain(code).unwrap_or("(no explanation registered)")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes the source can emit: every string literal passed as the
    /// first argument to `ValidationError::new(` or the local `err(`
    /// closures in the modules that produce validation errors.
    fn emitted_codes() -> Vec<String> {
        let sources = [
            include_str!("repo.rs"),
            include_str!("validate.rs"),
            include_str!("expand.rs"),
            include_str!("grants.rs"),
        ];
        let mut codes = Vec::new();
        for src in sources {
            for marker in ["ValidationError::new(", "err("] {
                for (at, _) in src.match_indices(marker) {
                    let rest = src[at + marker.len()..].trim_start();
                    if let Some(lit) = rest.strip_prefix('"') {
                        if let Some(end) = lit.find('"') {
                            codes.push(lit[..end].to_string());
                        }
                    }
                }
            }
        }
        codes.sort();
        codes.dedup();
        codes
    }

    #[test]
    fn every_emitted_code_is_explained_and_vice_versa() {
        let emitted = emitted_codes();
        let mut registered: Vec<&str> = CODES.iter().map(|(c, _)| *c).collect();
        registered.sort();
        let unexplained: Vec<&String> = emitted.iter().filter(|c| explain(c).is_none()).collect();
        assert!(
            unexplained.is_empty(),
            "codes emitted without an entry in CODES: {unexplained:?}"
        );
        let dead: Vec<&&str> = registered
            .iter()
            .filter(|c| !emitted.contains(&c.to_string()))
            .collect();
        assert!(dead.is_empty(), "CODES entries nothing emits: {dead:?}");
        assert_eq!(registered.len(), CODES.len(), "duplicate code in CODES");
    }

    #[test]
    fn explanations_are_one_per_distinct_code_in_code_order() {
        let errors = vec![
            ValidationError::new("zone-unknown-room", "z", "m", None),
            ValidationError::new("invalid-name", "a", "m", None),
            ValidationError::new("invalid-name", "b", "m", None),
        ];
        let lines = explanations(&errors);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("invalid-name: "));
        assert!(lines[1].starts_with("zone-unknown-room: "));
    }
}
