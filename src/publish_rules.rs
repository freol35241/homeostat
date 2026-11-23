//! Checks on what a unit may or must publish, over the expanded keys and not
//! the grant table. An
//! entity-addressed key names a bound entity, two forecasters do not share
//! one key, reserved classes stay with their owners, an arbiter-class publish
//! covers every arbitrated entity, and every declared aspect is published.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Code, Diagnostics};
use crate::expand::{publishes, Direction, ExpandedKey};
use crate::keyspace::{KeyExpr, Segment};
use crate::manifest::{UnitKind, WriteMode};
use crate::repo::House;

/// Arbitration coverage. An arbitrated entity that no arbiter-class publish
/// reaches would never receive a write token, because the arbiter service has
/// no path to it. This mirrors `check_exclusive_writers`, but works over
/// `expanded` directly, since arbiter-class publishes do not form cmd-class
/// grants.
pub(crate) fn check_arbitrated_coverage(
    house: &House,
    expanded: &[ExpandedKey],
    diags: &mut Diagnostics,
) {
    for entity in &house.entities {
        if entity.file.write_policy.mode() != WriteMode::Arbitrated {
            continue;
        }
        let prefix = [
            "home",
            "arbiter",
            entity.file.entity.room.as_str(),
            entity.name.as_str(),
        ];
        if !publishes(expanded, None, &prefix) {
            diags.in_file(&entity.path).error(
                Code::ArbitratedUncovered,
                &entity.name,
                format!(
                    "arbitrated entity \"{}\" has no arbiter-class publish covering it",
                    entity.name
                ),
            );
        }
    }
}

/// An entity-addressed key must belong to a bound entity. A templated publish
/// always does. A concrete one must name a bound entity's room and name
/// literally. Without this check a virtual sensor could publish under any
/// free-form key. `forecast` follows the same rule as `state`, because a
/// forecast extends the same series forward and so uses the same entity's key
/// space.
pub(crate) fn check_entity_keys_bound(
    house: &House,
    expanded: &[ExpandedKey],
    diags: &mut Diagnostics,
) {
    for key in expanded {
        if key.direction != Direction::Publishes || key.templated {
            continue;
        }
        for expr in &key.exprs {
            let Some(class @ ("state" | "forecast")) = expr.class() else {
                continue;
            };
            let forecast = class == "forecast";
            let code = if forecast {
                Code::ForecastPublishUnbound
            } else {
                Code::StatePublishUnbound
            };
            let unit = house
                .unit(&key.unit)
                .expect("expanded key from loaded unit");
            let subject = format!("{}.{}", key.unit, key.entry);
            let (room, entity) = (expr.0.get(2), expr.0.get(3));
            let (Some(Segment::Literal(room)), Some(Segment::Literal(entity))) = (room, entity)
            else {
                let message = format!(
                    "{class} publish \"{}\" needs literal room and entity segments (or {{room}}/{{entity}} templates)",
                    key.source
                );
                diags.in_file(&unit.path).error(code, subject, message);
                continue;
            };
            // State needs the binding unit, because each entity has one master
            // and the registry records it. A forecast does not. It is a
            // source's claim about a series' future values, and the best
            // source is often not the binder. A weather service forecasts an
            // outdoor sensor it does not own. A controller forecasts the
            // trajectory of a device it commands, and the grant graph means it
            // cannot also bind that device. The entity must still exist, so
            // that the recorder and the dashboard show the value.
            let declared = house
                .entities
                .iter()
                .find(|e| &e.name == entity && &e.file.entity.room == room);
            let bound = match declared {
                None => false,
                Some(e) => forecast || e.owner == key.unit,
            };
            if !bound {
                let message = if forecast {
                    format!(
                        "forecast key \"home/forecast/{room}/{entity}/…\" names no entity in the house"
                    )
                } else {
                    format!(
                        "state key \"home/state/{room}/{entity}/…\" is not under an entity bound by \"{}\"",
                        key.unit
                    )
                };
                diags.in_file(&unit.path).error(code, subject, message);
            }
        }
    }
}

/// One slot of a forecast publish (aspect or source): a literal, or any value
/// when the expression wildcards it.
#[derive(PartialEq)]
enum Slot {
    Exact(String),
    Any,
}

impl Slot {
    /// Whether two publishes can land on the same key at this slot.
    fn compatible(&self, other: &Slot) -> bool {
        match (self, other) {
            (Slot::Exact(a), Slot::Exact(b)) => a == b,
            _ => true,
        }
    }
}

/// Each forecast series has one publisher, where "series" includes the source.
/// Several providers may forecast one aspect, which is what the source segment
/// is for. Two units must not write the same source's key: the mirror keeps
/// only the last document per key, so the second would overwrite the first.
///
/// A publish may wildcard its aspect or source slot, so entries are compared
/// pairwise instead of grouped by key. Two publishes collide when, at every
/// slot, they agree or one of them accepts anything.
pub(crate) fn check_forecast_conflicts(expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    let slot = |expr: &KeyExpr, i: usize| -> Slot {
        // `**` anywhere from the aspect slot on stands for every slot
        // after it, so a key that has one accepts anything here.
        if expr.0.iter().skip(4).any(|s| matches!(s, Segment::AnyRec)) {
            return Slot::Any;
        }
        match expr.0.get(i) {
            Some(Segment::Literal(v)) => Slot::Exact(v.clone()),
            _ => Slot::Any,
        }
    };
    let mut forecast_publishes: Vec<(&str, String, String, Slot, Slot)> = Vec::new();
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        for expr in &key.exprs {
            if expr.class() != Some("forecast") {
                continue;
            }
            let (Some(Segment::Literal(room)), Some(Segment::Literal(entity))) =
                (expr.0.get(2), expr.0.get(3))
            else {
                continue;
            };
            forecast_publishes.push((
                key.unit.as_str(),
                room.clone(),
                entity.clone(),
                slot(expr, 4),
                slot(expr, 5),
            ));
        }
    }
    let mut reported: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (i, a) in forecast_publishes.iter().enumerate() {
        for b in forecast_publishes.iter().skip(i + 1) {
            if a.0 == b.0 || a.1 != b.1 || a.2 != b.2 {
                continue;
            }
            if !a.3.compatible(&b.3) || !a.4.compatible(&b.4) {
                continue;
            }
            let pair = if a.0 < b.0 { (a.0, b.0) } else { (b.0, a.0) };
            if !reported.insert(pair) {
                continue;
            }
            diags.error(
                Code::ForecastPublishConflict,
                format!("home/forecast/{}/{}", a.1, a.2),
                format!(
                    "{} and {} publish forecasts that land on the same key; \
                     give each source its own segment, or one overwrites the other",
                    pair.0, pair.1
                ),
            );
        }
    }
}

/// Reserved classes (docs/design.md#reserved-classes). `config` and `meta`
/// belong to the core. `health`, `discovery`, `hold` and `arbitration` are
/// per unit, under
/// the publishing unit's own name. `arbiter`, `clock` and `history` are each
/// one service's output. The SDK only checks that a published key is within a
/// declared expression. Without this check an automation could declare
/// `home/arbiter/**` and forge post-arbitration commands, or another unit's
/// discovery record or holds, with an empty grant table.
pub(crate) fn check_reserved_classes(
    house: &House,
    expanded: &[ExpandedKey],
    diags: &mut Diagnostics,
) {
    let mut singleton_publishers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        if matches!(key.class, "arbiter" | "clock" | "history") {
            singleton_publishers
                .entry(key.class)
                .or_default()
                .insert(key.unit.as_str());
        }
    }
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        let class = key.class;
        let next = key.source.split('/').nth(2);
        let unit = house
            .unit(&key.unit)
            .expect("expanded key from loaded unit");
        let message = match class {
            "config" | "meta" => Some(format!(
                "\"{}\" publishes under home/{class}/, which only the core writes",
                key.source
            )),
            "health" | "discovery" | "hold" | "arbitration" if next != Some(key.unit.as_str()) => {
                Some(format!(
                    "\"{}\" must sit under this unit's own name: home/{class}/{}/...",
                    key.source, key.unit
                ))
            }
            "arbiter" | "clock" | "history" => {
                let publishers = &singleton_publishers[class];
                if unit.manifest.unit.kind != UnitKind::Service {
                    Some(format!(
                        "\"{}\": only a service may publish under home/{class}/",
                        key.source
                    ))
                } else if publishers.len() > 1 {
                    let others: Vec<&str> = publishers
                        .iter()
                        .copied()
                        .filter(|u| *u != key.unit)
                        .collect();
                    Some(format!(
                        "\"{}\": home/{class}/ is also published by {}; exactly one service owns it",
                        key.source,
                        others.join(", ")
                    ))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(message) = message {
            diags.in_file(&unit.path).error(
                Code::ReservedClassPublish,
                format!("{}.{}", key.unit, key.entry),
                message,
            );
        }
    }
}

/// Checks every `[aspects]` entry against what its entity's owner publishes.
/// A misspelt aspect would otherwise sit there unused, and an interval total
/// over the real aspect would say only that it declares no interval. A unit
/// publishes only through its `[bus.publishes]`, whatever its kind, so every
/// owner is checked. An owner that publishes its entities' aspects under a
/// wildcard, as most adapters do, covers any name. A name that is not a key
/// segment is `invalid-name` already and is not checked again.
pub(crate) fn check_aspects(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    for entity in &house.entities {
        for aspect in entity.file.aspects.iter().flat_map(|a| a.keys()) {
            let segments = [
                "home",
                "state",
                &entity.file.entity.room,
                &entity.name,
                aspect,
            ];
            let key = segments.join("/");
            if !crate::validate::valid_segment(aspect) {
                continue;
            }
            if !publishes(expanded, Some(&entity.owner), &segments) {
                diags.in_file(&entity.path).error(
                    Code::AspectUnpublished,
                    format!("{}.{aspect}", entity.name),
                    format!(
                        "\"{}\" does not publish {key}; [aspects] names an aspect its owner publishes",
                        entity.owner
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expand::expand;
    use crate::grants::resolve;
    use crate::test_houses::*;

    #[test]
    fn arbitrated_entity_with_no_arbiter_publish_is_a_plan_error() {
        let house = house_with_lock(false);
        let (expanded, _warnings, expand_errors) = collect(|d| expand(&house, d));
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (_grants, _warnings, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(
            errors
                .iter()
                .any(|e| e.code == Code::ArbitratedUncovered && e.subject == "lock"),
            "{errors:?}"
        );
    }

    /// Two providers can forecast one aspect without conflict. Without the
    /// source in the key, the two would write one key.
    #[test]
    fn two_providers_may_forecast_one_aspect_under_their_own_sources() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[
                ("smhi", "home/forecast/global/outdoor_temp/temperature/smhi"),
                ("yr", "home/forecast/global/outdoor_temp/temperature/yr"),
            ],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A wildcard slot accepts anything, so a unit publishing the whole
    /// aspect space collides with one naming a single source inside it.
    #[test]
    fn a_wildcard_forecast_publish_collides_with_a_named_source() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[
                ("broad", "home/forecast/global/outdoor_temp/**"),
                ("smhi", "home/forecast/global/outdoor_temp/temperature/smhi"),
            ],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert_eq!(
            errors.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![Code::ForecastPublishConflict],
        );
    }

    /// Builds a house where `forecaster` units each publish one concrete
    /// forecast key, and `esphome` owns the entity being forecast.
    fn house_forecasting(entity_name: &str, room: &str, forecasters: &[(&str, &str)]) -> House {
        let forecasters: Vec<(&str, String)> = forecasters
            .iter()
            .map(|&(name, key)| {
                let manifest = format!(
                    r#"
                    schema = 1

                    [unit]
                    name = "{name}"
                    kind = "automation"

                    [runtime]
                    command = "true"
                    restart = "always"

                    [bus.publishes.curve]
                    key = "{key}"
                "#
                );
                (name, manifest)
            })
            .collect();
        let mut units = vec![("esphome", ESPHOME)];
        units.extend(
            forecasters
                .iter()
                .map(|(name, text)| (*name, text.as_str())),
        );
        let sensor = format!(
            r#"
            schema = 1

            [entity]
            id = "{entity_name}"
            capability = "sensor"
            room = "{room}"

            [write_policy]
            mode = "shared"
            owner = "esphome"
        "#
        );
        House::from_toml(&units, &[("esphome", entity_name, &sensor)])
    }

    /// A forecast may name an entity this unit does not bind; state may not. A
    /// forecast is a source's claim about a series' future values, and the
    /// best source is often not the binder. A weather service forecasts a
    /// sensor an adapter owns. A controller forecasts a device it commands,
    /// and because the grant graph runs from automation to device, it cannot
    /// also bind it.
    #[test]
    fn a_forecast_may_name_an_entity_another_unit_owns() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[(
                "weather",
                "home/forecast/global/outdoor_temp/temperature/smhi",
            )],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// The entity must still exist. Dropping the ownership rule does not allow
    /// free-form keys: a forecast under a name no entity describes would be
    /// recorded but would not appear on any generated surface.
    #[test]
    fn a_forecast_for_no_entity_is_still_refused() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[("weather", "home/forecast/global/nowhere/temperature/smhi")],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert_eq!(
            errors.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![Code::ForecastPublishUnbound],
        );
    }

    /// One source, one publisher. Ownership would guarantee this, since each
    /// entity has one binder, but a forecast does not require ownership. So
    /// the property is checked directly. The mirror keeps only the last
    /// document per key, so a second publisher on the same source would
    /// overwrite the first. Two providers under their own source segments are
    /// allowed; a test below covers that.
    #[test]
    fn two_units_may_not_publish_one_forecast_source() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[
                (
                    "smhi",
                    "home/forecast/global/outdoor_temp/temperature/consensus",
                ),
                (
                    "yr",
                    "home/forecast/global/outdoor_temp/temperature/consensus",
                ),
            ],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        let conflict = errors
            .iter()
            .find(|e| e.code == Code::ForecastPublishConflict)
            .expect("two forecasters on one series must conflict");
        assert!(
            conflict.message.contains("smhi") && conflict.message.contains("yr"),
            "the conflict must name both publishers: {}",
            conflict.message
        );
    }
}
