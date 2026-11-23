//! Houses and a diagnostics helper that the grant, publish-rule and wiring
//! unit tests share.

use crate::error::{Diagnostics, ValidationError, Warning};
use crate::repo::House;

/// Runs one stage into a fresh sink and returns its output beside the
/// warnings and errors it reported.
pub(crate) fn collect<T>(
    stage: impl FnOnce(&mut Diagnostics) -> T,
) -> (T, Vec<Warning>, Vec<ValidationError>) {
    let mut diags = Diagnostics::default();
    let out = stage(&mut diags);
    (out, diags.warnings, diags.errors)
}

pub(crate) const ZIGBEE: &str = r#"
    schema = 1

    [unit]
    name = "zigbee"
    kind = "adapter"

    [runtime]
    command = "true"
    restart = "always"

    [entities]
    dir = "entities/zigbee/"

    [bus.subscribes]
    commands = "home/cmd/{room}/{entity}/**"
    arbiter_commands = "home/arbiter/{room}/{entity}/**"
"#;

pub(crate) const NIGHT_MODE: &str = r#"
    schema = 1

    [unit]
    name = "night_mode"
    kind = "automation"

    [runtime]
    command = "true"
    restart = "always"

    [bus.publishes.lock]
    key = "home/cmd/hallway/lock/lock"
    capability = "lock"
    priority = "automation"
"#;

pub(crate) const ARBITER: &str = r#"
    schema = 1

    [unit]
    name = "arbiter"
    kind = "service"

    [runtime]
    command = "true"
    restart = "always"

    # A service (not an adapter) cannot use {room}/{entity} templates, so
    # it covers arbitrated entities with a plain wildcard instead.
    [bus.publishes.forwarded]
    key = "home/arbiter/**"
"#;

pub(crate) const LOCK_ENTITIES: &[(&str, &str, &str)] = &[
    (
        "zigbee",
        "lamp",
        r#"
            schema = 1

            [entity]
            id = "lamp"
            capability = "light"
            room = "kitchen"

            [write_policy]
            mode = "shared"
            owner = "zigbee"
        "#,
    ),
    (
        "zigbee",
        "lock",
        r#"
            schema = 1

            [entity]
            id = "lock"
            capability = "lock"
            room = "hallway"

            [write_policy]
            mode = "arbitrated"
            owner = "zigbee"
        "#,
    ),
];

/// Builds a house with an arbitrated lock and a shared lamp, both bound
/// to adapter "zigbee", plus an automation wishing to command the lock.
/// `arbiter` lets each test decide whether an arbiter-class publish
/// covers the lock.
pub(crate) fn house_with_lock(arbiter: bool) -> House {
    let mut units = vec![("zigbee", ZIGBEE), ("night_mode", NIGHT_MODE)];
    if arbiter {
        units.push(("arbiter", ARBITER));
    }
    House::from_toml(&units, LOCK_ENTITIES)
}

/// An adapter that binds sensors and publishes nothing the tests read.
pub(crate) const ESPHOME: &str = r#"
    schema = 1

    [unit]
    name = "esphome"
    kind = "adapter"

    [runtime]
    command = "true"
    restart = "always"

    [entities]
    dir = "entities/esphome/"
"#;
