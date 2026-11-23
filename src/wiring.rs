//! Device feeds (`[inputs]`) and declared sources (`[sources]`). Both name
//! another entity's aspect, and both resolve it the same way: to its state
//! key and the unit that binds it. The plan prints them beside the grant
//! table.

use crate::error::{Code, Diagnostics, ValidationError};
use crate::expand::{publishes, subscribes, ExpandedKey};
use crate::manifest::UnitKind;
use crate::repo::{House, LoadedEntity};

/// An aspect of another entity that a feed or a declared source names,
/// resolved to its state key and its entity's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributor {
    /// The contributing entity.
    pub entity: String,
    /// The contributing aspect.
    pub aspect: String,
    /// The unit that binds the contributing entity.
    pub owner: String,
    /// The state key the aspect is read from,
    /// `home/state/{room}/{entity}/{aspect}`.
    pub key: String,
}

/// The block that names a contributor. The two raise the same errors under
/// different codes and wording.
enum Block {
    Inputs,
    Sources,
}

/// Resolves `aspect` of the entity `contributing`, which `entity` names as
/// `name` in its `[inputs]` or `[sources]`. The contributing entity must exist, and
/// an automation-owned one must publish the aspect. The key's segments come
/// back beside the contributor, for a caller that matches them against a
/// unit's subscriptions.
fn resolve_contributor<'h>(
    house: &'h House,
    expanded: &[ExpandedKey],
    entity: &LoadedEntity,
    name: &str,
    contributing: &str,
    aspect: &'h str,
    block: Block,
) -> Result<(Contributor, [&'h str; 5]), ValidationError> {
    let (unknown_entity, unpublished_aspect, entity_noun, key_noun) = match block {
        Block::Inputs => (
            Code::InputUnknownEntity,
            Code::InputUnpublishedAspect,
            "source entity",
            "a fed aspect",
        ),
        Block::Sources => (
            Code::SourceUnknownEntity,
            Code::SourceUnpublishedAspect,
            "contributing entity",
            "a declared source",
        ),
    };
    let subject = format!("{}.{name}", entity.name);
    let file = Some(entity.path.clone());
    let Some(src) = house.entities.iter().find(|e| e.name == contributing) else {
        return Err(ValidationError::new(
            unknown_entity,
            subject,
            format!("{entity_noun} \"{contributing}\" does not exist"),
            file,
        ));
    };
    let segments = ["home", "state", &src.file.entity.room, &src.name, aspect];
    let key = segments.join("/");
    let src_is_automation = house
        .unit(&src.owner)
        .is_some_and(|u| u.manifest.unit.kind == UnitKind::Automation);
    if src_is_automation && !publishes(expanded, Some(&src.owner), &segments) {
        return Err(ValidationError::new(
            unpublished_aspect,
            subject,
            format!(
                "\"{}\" does not publish {key}; {key_noun} must be in its owner's [bus.publishes]",
                src.owner
            ),
            file,
        ));
    }
    let contributor = Contributor {
        entity: src.name.clone(),
        aspect: aspect.to_string(),
        owner: src.owner.clone(),
        key,
    };
    Ok((contributor, segments))
}

/// One resolved feed: a device input wired to a source aspect
/// (docs/design.md#device-feeds). The plan renders it next to the grant table.
/// A feed is not a walk-order edge, because a control loop that reads a device
/// and feeds a term back to it is a legitimate cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    /// The fed entity.
    pub entity: String,
    /// The adapter's name for the input.
    pub input: String,
    /// The source, resolved.
    pub source: Contributor,
}

/// Resolves every `[inputs]` block. It checks that the source exists, that the
/// target is a device, and that an automation-owned source publishes the
/// aspect.
pub(crate) fn resolve_feeds(
    house: &House,
    expanded: &[ExpandedKey],
    diags: &mut Diagnostics,
) -> Vec<Feed> {
    let mut feeds = Vec::new();
    for entity in &house.entities {
        let Some(inputs) = &entity.file.inputs else {
            continue;
        };
        let owner_is_automation = house
            .unit(&entity.owner)
            .is_some_and(|u| u.manifest.unit.kind == UnitKind::Automation);
        if owner_is_automation {
            diags.in_file(&entity.path).error(
                Code::VirtualEntityFed,
                &entity.name,
                "automation-owned entities have no device inputs to feed",
            );
            continue;
        }
        for (input, source) in inputs {
            match resolve_contributor(
                house,
                expanded,
                entity,
                input,
                &source.entity,
                &source.aspect,
                Block::Inputs,
            ) {
                Ok((contributor, _)) => feeds.push(Feed {
                    entity: entity.name.clone(),
                    input: input.clone(),
                    source: contributor,
                }),
                Err(error) => diags.errors.push(error),
            }
        }
    }
    feeds
}

/// One resolved `[sources]` entry: a contributor to a computed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The computed entity.
    pub entity: String,
    /// The name the computed entity gives this contributor.
    pub name: String,
    /// The contributor, resolved.
    pub contributor: Contributor,
    /// This contributor's own caveat, as declared.
    pub note: Option<String>,
}

/// Resolves every `[sources]` block. It makes the same two checks as
/// `[inputs]`, for the same reason: the contributor exists, and an
/// automation-owned contributor publishes the aspect. It also warns when the
/// unit does not subscribe a source it claims to derive from. Such a unit is
/// either mis-declared or has dead code, and the overlay would draw a line
/// that plays no part in the value.
pub(crate) fn resolve_sources(
    house: &House,
    expanded: &[ExpandedKey],
    diags: &mut Diagnostics,
) -> Vec<Source> {
    let mut sources = Vec::new();
    for entity in &house.entities {
        let Some(declared) = &entity.file.sources else {
            continue;
        };
        for (name, source) in declared {
            let (contributor, segments) = match resolve_contributor(
                house,
                expanded,
                entity,
                name,
                &source.entity,
                &source.aspect,
                Block::Sources,
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    diags.errors.push(error);
                    continue;
                }
            };
            if !subscribes(expanded, Some(&entity.owner), &segments) {
                diags.in_file(&entity.path).warn(
                    format!("{}.{name}", entity.name),
                    format!(
                        "\"{}\" declares {} as a source but does not subscribe it",
                        entity.owner, contributor.key
                    ),
                );
            }
            sources.push(Source {
                entity: entity.name.clone(),
                name: name.clone(),
                contributor,
                note: source.note.clone(),
            });
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expand::expand;
    use crate::test_houses::*;

    /// A house where `fusion` owns a computed entity derived from one sensor
    /// an adapter owns. `subscribes` decides whether the fusion reads what it
    /// claims to.
    fn house_with_sources(source_aspect: &str, subscribes: Option<&str>) -> House {
        let subscribes = subscribes.map_or_default(|expr| format!(r#"inputs = "{expr}""#));
        let fusion = format!(
            r#"
            schema = 1

            [unit]
            name = "fusion"
            kind = "automation"

            [runtime]
            command = "true"
            restart = "always"

            [entities]
            dir = "entities/fusion/"

            [bus.subscribes]
            {subscribes}

            [bus.publishes.fused]
            key = "home/state/global/fused_temp/temperature"
        "#
        );
        let outdoor_temp = r#"
            schema = 1

            [entity]
            id = "outdoor_temp"
            capability = "sensor"
            room = "global"

            [write_policy]
            mode = "shared"
            owner = "esphome"
        "#;
        let fused_temp = format!(
            r#"
            schema = 1

            [entity]
            capability = "sensor"
            room = "global"

            [write_policy]
            mode = "shared"
            owner = "fusion"

            [sources.station]
            entity = "outdoor_temp"
            aspect = "{source_aspect}"
        "#
        );
        House::from_toml(
            &[("esphome", ESPHOME), ("fusion", &fusion)],
            &[
                ("esphome", "outdoor_temp", outdoor_temp),
                ("fusion", "fused_temp", &fused_temp),
            ],
        )
    }

    /// A declared source resolves to the contributor's own state key. The
    /// entity keeps publishing where it did, and the declaration only refers
    /// to it. It is not a second place for the value.
    #[test]
    fn a_declared_source_resolves_to_the_contributors_key() {
        let house = house_with_sources(
            "temperature",
            Some("home/state/global/outdoor_temp/temperature"),
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (sources, warnings, errors) = collect(|d| resolve_sources(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            sources,
            vec![Source {
                entity: "fused_temp".to_string(),
                name: "station".to_string(),
                contributor: Contributor {
                    entity: "outdoor_temp".to_string(),
                    aspect: "temperature".to_string(),
                    owner: "esphome".to_string(),
                    key: "home/state/global/outdoor_temp/temperature".to_string(),
                },
                note: None,
            }]
        );
    }

    /// The declaration is checked. A unit that claims to derive from a reading
    /// it does not subscribe is either mis-declared or has dead code, and the
    /// overlay would draw a contributor line that plays no part.
    #[test]
    fn a_source_the_owner_does_not_subscribe_warns() {
        let house = house_with_sources("temperature", None);
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (sources, warnings, errors) = collect(|d| resolve_sources(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(sources.len(), 1, "the source still resolves");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not subscribe"),
            "{}",
            warnings[0]
        );
    }

    /// A contributor that does not exist is an error, not a warning: the
    /// overlay has no series to draw and the declaration names nothing.
    #[test]
    fn a_source_naming_no_entity_is_refused() {
        let mut house = house_with_sources("temperature", None);
        let fused = house
            .entities
            .iter_mut()
            .find(|e| e.name == "fused_temp")
            .unwrap();
        fused
            .file
            .sources
            .as_mut()
            .unwrap()
            .get_mut("station")
            .unwrap()
            .entity = "ghost".to_string();
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve_sources(&house, &expanded, d));
        assert_eq!(
            errors.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![Code::SourceUnknownEntity],
        );
    }
}
