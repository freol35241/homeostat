# Homeostat design

How homeostat works, and why it is built that way. The document
describes the system as it is. A change that makes a sentence here
untrue fixes the sentence in the same commit. How each decision was
reached is in the git history.

Field-by-field references live beside it: the manifest contract in
[manifest.md](manifest.md) (generated from the parser), the adapter
contract in [adapters.md](adapters.md), and the dashboard's widgets in
[widgets.md](widgets.md).

1. [What homeostat is](#what-homeostat-is)
2. [Architecture](#architecture)
3. [Glossary](#glossary)
4. [Key space](#key-space)
5. [Units and manifests](#units-and-manifests)
6. [Capabilities, grants and write policy](#capabilities-grants-and-write-policy)
7. [Plan and apply](#plan-and-apply)
8. [Supervision](#supervision)
9. [Live parameters](#live-parameters)
10. [State, history and forecasts](#state-history-and-forecasts)
11. [Derived values](#derived-values)
12. [Health, availability and the audit trail](#health-availability-and-the-audit-trail)
13. [Surfaces](#surfaces)
14. [Security model](#security-model)
15. [Distribution](#distribution)
16. [Open questions](#open-questions)

## What homeostat is

Homeostat runs a household from a git repository. It is named after
W. Ross Ashby's 1948 machine. Like that machine, it is a regulator that
holds the house in equilibrium; it is not an assistant waiting for
commands. The family adjusts setpoints and the owner governs the
regulating machinery. The code draws the same line: parameters are
editable by the family at runtime, and structure changes only through
the repo and [plan and apply](#plan-and-apply).

It replaces Home Assistant for one owner's actual devices (Zigbee
through Zigbee2MQTT, ESPHome, MQTT). It does not try to cover the long
tail. Its aims:

- Text configuration first. The house repo is the system of record.
  No UI mutates hidden state. The running system is derived from the
  repo plus the current state of the world.
- Automations are plain Python scripts against a small SDK. There is
  no DSL to outgrow.
- Agents maintain it the way people do. An agent reads the same files
  and runs the same `plan`, and its changes reach the house through
  the same commit-and-apply path as a human's.
- A small core. The core owns the key space, validation, the grant
  table, plan and apply, and supervision. Anything that speaks a
  device protocol or keeps data lives in a unit outside it.

There is no Home Assistant bridge.

## Architecture

There are four parts: a Rust core, a Zenoh bus, the units the core
supervises, and the house repo they are all derived from.

```
house repo (git) --plan/apply--> core: homeostat up
                                 (bus router, supervisor, last-value caches)
                                        |
                              Zenoh bus, home/**
        +---------------+---------------+---------------+
     adapters      automations       services        observers
        |                          recorder, arbiter,  CLI, tests
     devices                       clock, dashboard,
  (MQTT broker,                    mcp
   ESPHome API, ...)
```

### Core

The core is the `homeostat` binary. Offline, `homeostat plan` validates
a house repo, expands templates and zones, resolves the grant table and
prints a plan. Running, `homeostat up` is the bus router and the
process supervisor. It also owns the live parameter store and the
last-value mirrors, and it executes apply walks. `homeostat mcp` serves
the read-only agent surface. The core parses no device protocol and
keeps no history.

### Bus

The bus is Zenoh. Pub/sub carries state and commands. Queryables answer
reads (last values, history, parameters) and the two writes that need a
synchronous answer: a parameter write and an apply. The supervisor's
session is a router on a fixed endpoint. Every unit and observer
connects to it as a client with scouting off, so the topology is
explicit and parallel test buses never find each other. Keys and
payloads are described in [Key space](#key-space).

MQTT device traffic stays on its own broker (mosquitto), and only the
adapters that speak MQTT reach it. Homeostat does not use the Zenoh
MQTT plugin or bridge, for three reasons. Broker retain is
load-bearing (an OwnTracks phone's last position, Zigbee2MQTT's device
inventory), and the plugin does not document it. Mapping device topics
into `home/**` would stop adapters being the only boundary between a
device dialect and the bus. And plugin mode would parse foreign
protocols inside the supervisor.

### Units

Every running thing besides the core is a unit: an `adapter`,
`automation` or `service` ([Unit kinds](#unit-kinds)). One manifest
schema declares everything a unit may touch ([Units and
manifests](#units-and-manifests)). Python units are `uv run` scripts
whose dependencies are a PEP 723 block in the script, so each has its
own hermetic environment. Rust units are binaries. The SDK
(`sdk/python`, package `homeostat`) gives a Python unit its session,
typed key builders, the automation `Context` and the command envelope.

### Process model

Units are plain OS processes supervised by the core
([Supervision](#supervision)), in the Erlang tradition. The process
boundary gives fault isolation and a language boundary; it is not meant
as a microservice. Units are not containerized individually. The whole
system may run in one container ([Distribution](#distribution)).

### The house repo is the system of record

Nothing running edits the repo's manifests, entity files,
`zones.toml` or `dashboard.toml`. The only things written into the
checkout are a unit's own data (the recorder's store under the
gitignored `data/`) and pending plans under `plans/`. The running world
is diffed against the repo, not the other way round. A change is
a commit followed by [plan and apply](#plan-and-apply). A live
parameter edit is drift that the next apply resets
([Live parameters](#live-parameters)).
`home/meta/system/applied_commit` names the commit that is running.

### Unit granularity: the atom is the unit

The unit is the atom of **authority**: its grants, subscriptions,
publishes and parameters. It is also the atom of **failure** (its
liveliness token, backoff, breaker and process group) and of
**change** (its `files_hash` and its step in the apply walk). The
process model exists so that these three boundaries coincide on one
process.

What a unit contains is up to its author. A script may host several
rules, and its manifest declares the union of what they need.

- Group rules by shared blast radius rather than size. Rules belong in
  one unit when they should live and die together. They share a
  restart and a health key, and an edit to one is a behavioral change
  to all. "Evening lighting" and "the heat-pump setback" are two units,
  however small.
- Authority is the union. The grant table sees units, not rules. A
  rule that must not touch what its neighbour touches needs its own
  unit.
- Cost. A minimal Python unit uses about 12 MB resident, so forty use
  about 0.5 GB. That is fine on a NUC or a Pi 4. If memory becomes a
  constraint, bundle rules by the blast-radius rule above.

Homeostat has no multi-tenant automation runner (one service hosting
many rules with a scheduler). Such a runner shares authority and
failure across rules that did not choose to share them. Adding
per-rule health and restart to it would rebuild the supervisor in
Python.

## Glossary

These are the words this document, the code and the error messages
use, each in one sense. Where a word has two senses, both are listed.

- **House.** One home's configuration, and the system running it.
- **House repo.** The git repository holding a house as text: unit
  manifests, entity files, `zones.toml`, `dashboard.toml`. It is the
  system of record that the running world is diffed against.
- **Core.** The `homeostat` binary: validation, plan and apply, the
  supervisor, the last-value caches, the MCP server.
- **Unit.** Any running thing other than the core. It is one process
  with one manifest in `units/`. An **adapter** puts devices on the
  bus, an **automation** regulates, and a **service** is infrastructure
  with no entities. See [Unit kinds](#unit-kinds).
- **Entity.** One thing in the house with state, declared by an entity
  file. Its name is the file stem and is unique across the house.
- **Binding.** (1) The relation between an entity and the one unit that
  embodies it: an adapter for a device, an automation for a virtual
  entity. That unit is the entity's **owner**. (2) A **binding name**:
  the key of an entry in a manifest's `[bus.subscribes]` or
  `[bus.publishes]`. The SDK takes a binding name instead of a key
  expression.
- **Capability.** What kind of thing an entity is (`light`, `lock`,
  `sensor`, …), from a fixed vocabulary. It decides the base aspect,
  which commands can be granted, and the dashboard widget.
- **Aspect.** One named value of an entity, such as `on`, `brightness`
  or `temperature`. It is the last segment of a state key.
- **Base aspect.** The aspect a capability's commands and widget act
  on (`on` for a light). A capability without one takes no commands.
- **Feature.** An optional aspect beyond the base that an entity file
  declares (`brightness` on a light).
- **Aspect descriptor.** An adapter's description of an entity's
  aspects (label, kind, unit, group, commands), carried in its
  discovery record. See [Aspect descriptors](#aspect-descriptors).
- **Room.** Where an entity is; a key segment. The **pseudo-rooms**
  `global` and `person` hold entities with no place.
- **Zone.** A named set of rooms, written in a key's room slot and
  expanded at plan time. Zones never appear in a published key.
- **Key class.** The second segment of every bus key: `state`, `cmd`,
  `arbiter`, `forecast`, `config`, `meta`, `health`, `clock`,
  `history`, `discovery`, `hold`. See [Key space](#key-space).
- **Grant.** A unit's publish resolved against the entities it reaches.
  The **grant table** is the set of all grants. It is the permission
  record `plan` shows and the dependency graph that orders an apply.
- **Band.** A command's priority: `automation` < `agent` < `family` <
  `manual`. It is declared per publish in the manifest, not per
  command.
- **Write mode.** How commands to an entity are governed: `shared`,
  `exclusive` or `arbitrated`. See
  [Capabilities, grants and write policy](#capabilities-grants-and-write-policy).
- **Envelope.** A command's payload: `{value, priority, actor, id}`.
  The SDK stamps the priority from the manifest and the actor with the
  unit.
- **Arbiter.** The service that orders commands to arbitrated entities.
  It grants a **hold** (a lease per entity and aspect) to the winning
  band. See [Arbitrated mode](#arbitrated-mode).
- **Latch.** A commandable virtual entity: an automation-owned entity
  whose owner sets its state from the commands it receives.
- **Virtual entity.** An entity an automation owns. Its value is
  computed rather than read from a device.
- **Feed.** A device input wired to another entity's aspect, declared
  in the device's `[inputs]`. See [Device feeds](#device-feeds).
- **Source.** (1) In `[sources]`: a reading a computed value is derived
  from. See [Sources](#sources). (2) In a forecast key: the unit or
  provider that claims that future. See [Forecasts](#forecasts).
- **Forecast.** A series' future, published at `home/forecast/...`
  beside its present at `home/state/...`.
- **Live parameter.** A unit setting at `home/config/{unit}/{param}`.
  The core validates every write. Its `editable_by` tier, `owner` or
  `family`, says who may change it live. See
  [Live parameters](#live-parameters).
- **Liveliness token.** What a unit declares on the bus once it can do
  its job. "Up" means the token is present.
- **Health.** A unit's supervision status (`starting`, `running`,
  `backoff`, `open`, `stopped`). A **health event** is a unit's own
  report at `home/health/{unit}/event`. A **drop** is the event for
  input the unit refused.
- **Mirror.** The core's last-value cache. A late joiner reads it
  instead of waiting for the next publish. See
  [The last-value mirror](#the-last-value-mirror).
- **Plan.** The diff between the house repo and the running world,
  with its **tier**: `parameter-only`, `behavioral` or `structural`.
  **Apply** executes it. See [Plan and apply](#plan-and-apply).
- **Applied commit.** The house repo commit the running world was last
  applied from.
- **Discovery record.** What an adapter publishes about the devices its
  backend knows, bound or not. See [Discovery](#discovery).
- **Notable.** A reading the vocabulary marks as out of the ordinary.
  The dashboard lists it as a **deviation**.
- **Surface.** A way people or agents reach the house: the dashboard,
  the MCP server, notifications. See [Surfaces](#surfaces).

## Key space

Every key starts `home/{class}/`. The class list is fixed in
`src/keyspace.rs` (`CLASSES`). A manifest expression with any other
class is a plan error. Four classes are entity-addressed and share one
shape:

```
home/{class}/{room}/{entity}/{aspect}            state, cmd, arbiter
home/forecast/{room}/{entity}/{aspect}/{source}  forecast
```

A forecast key requires a `source`, and a state key has none
([Sources](#sources)). The other classes have their own shapes:

| Key | Written by | Carries |
|---|---|---|
| `home/state/{room}/{entity}/{aspect}` | the entity's owning unit | current value of one aspect |
| `home/cmd/{room}/{entity}/{aspect}` | any unit granted it | a command (a wish) |
| `home/arbiter/{room}/{entity}/{aspect}` | the arbiter | a command it let through to an arbitrated entity |
| `home/forecast/{room}/{entity}/{aspect}/{source}` | the source's unit | one issue of a series' future |
| `home/config/{unit}/{param}` | the core only | a live parameter value |
| `home/meta/{unit}/manifest_hash`, `files_hash`, `manifest`, `log` | the core | the unit as applied; its captured output |
| `home/meta/system/grants`, `applied_commit`, `about`, `apply` | the core | the grant table, the applied commit, the running version; the apply queryable |
| `home/health/{unit}` | the supervisor | the unit's supervision status |
| `home/health/{unit}/alive` | the unit | its liveliness token |
| `home/health/{unit}/event` | the unit | its health events |
| `home/clock/minute`, `home/clock/date` | the clock service | civil time |
| `home/discovery/{unit}` | an adapter | its view of its periphery |
| `home/hold/{unit}` | the arbiter | what it currently holds |
| `home/history/...` | (recorder queryables) | history reads |

### Rooms, entities and zones

- A key has one room segment. There are no floors or areas. The room
  is a field of the entity file. Entity names (the file stem) are
  unique across the house (`duplicate-entity-name`), so an entity can
  be addressed without its room.
- Entities with no place live in the pseudo-rooms `global` or
  `person`. These two, `home` and every class name are reserved words.
  A room may be a pseudo-room but no other reserved word
  (`reserved-room-name`). A zone may be none of them
  (`reserved-zone-name`). The unit name `system` is reserved for
  `home/meta/system/**`.
- Every name is one key segment. Unit, parameter, entity, room, zone
  and view names match `[A-Za-z0-9_.-]+` and are not `.` or `..`
  (`invalid-name`). Other characters would break the fixed shape
  (`/`), mean something to Zenoh (`*`, `$`, `?`, `#`) or invite
  encoding surprises. The SDK's key builders (`homeostat.keys`) apply
  the same rule at runtime. An adapter drops a device-chosen field name
  that fails it, with `malformed-payload`.
- Zones never appear in keys. A zone is a named set of rooms in
  `zones.toml`. A zone in an expression's room slot expands to one
  expression per member room. The expansion happens at plan time and
  identically in the SDK, so a unit subscribes to what `plan` printed.
  A zone may not share a room's name or contain a pseudo-room. An empty
  zone is a warning.
- `{room}` and `{entity}` templates expand once per entity the unit
  binds ([Key expansion](#key-expansion)).

A subscription can address by identity or by space.
`home/state/kitchen/**` is whatever is in the kitchen.
`home/state/*/that_lamp/**` is that lamp wherever it lives.

Moving an entity is an entity-file edit that goes through plan and
apply. The owner's `files_hash` changes and the walk restarts it.
Grants that reached the entity by room are re-resolved. History
survives the move because a series is keyed without its room. That is
why `home/history` keys have no room segment
([History and the recorder](#history-and-the-recorder)).

### Bus payload conventions

Payloads are UTF-8 JSON, except in the core's meta space:
`manifest_hash` and `files_hash` are hex text, `manifest` is the raw
TOML, and `applied_commit` is the commit as text.

State is one bare JSON scalar per aspect: a boolean, a number or a
string, with no wrapper. A composite reading is split into aspects (a
position is `lat` and `lon`). The mirror, the recorder and the
dashboard handle one scalar per key, and a key per aspect gives each
part its own history at no cost. An object, array or `null` on a state
key is a bug, and the recorder drops it (`non-scalar`).

Numbers must be finite. JSON has no NaN or Infinity, but Python's
`json` writes them. The SDK's `put_json` drops such a value with a
`drop` event (`non-finite`). The recorder drops one from any other
publisher, and the dashboard refuses one. No consumer has to guard
against them.

Times carry an offset. Clock payloads are local RFC3339
(`"2026-07-03T21:04:00+02:00"`), and the SDK refuses a naive forecast
timestamp. Recorder events and log lines use integer µs UTC.

Every `home/cmd` and `home/arbiter` payload is an envelope,
`{value, priority, actor, id}`. `priority` is one of `automation`,
`agent`, `family` or `manual` ([Cmd envelopes](#cmd-envelopes)).
Priority and actor are self-declared and are checked only for shape
([Local-only access](#local-only-access)). An adapter's
`parse_command` drops a non-JSON payload with `malformed-payload`. It
drops one that lacks `value` or a known `priority` with
`invalid-command`.

`put_json` sends `home/cmd` and `home/arbiter` puts with congestion
control `BLOCK` at priority `INTERACTIVE_HIGH`. Zenoh's default, `DROP`
at data priority, is right for the next temperature reading but wrong
for "unlock the door".

Commands are puts rather than queries, for three reasons. The outcome,
a readback on `home/state`, arrives long after a query would have
closed. Who is listening (the owner's liveliness token) and any refusal
or drop (events carrying `cmd_id`) arrive without a query. And the
recorder never sees a query. Queries are used where one owner answers
in full at once: parameter writes and apply.

A read query is a GET without payload. A write is a GET with a JSON
payload. A refused write gets an error reply (`{"error": "<message>"}`
from the config queryable). Mirror replies carry the value's stamp and
age ([The last-value mirror](#the-last-value-mirror)).

A class whose value is not one scalar carries one JSON document per
key: health status, a discovery record array, the arbiter's
`{"schema": 1, "holds": [...]}`, or a [forecast](#forecasts) issue
`{"schema": 1, "issued": ..., "points": [...]}`. Health events are
objects with a `kind` ([Health events](#health-events)).

### Timestamps

Every sample on the bus carries a Zenoh timestamp. The core's router
stamps each sample that arrives without one (`timestamping` in
`src/bus.rs`), so every subscriber sees the same stamp for a sample.
A stamp is the core's wall-clock time plus the ID of whoever set it,
and stamps order totally: by time, then by ID.

A publisher may set its own stamp to say when its value was true. Only
the core does this, when it
[replays recorded state after a restart](#replay-after-a-core-restart).
Its replays carry the ID `1`. The router's stamps carry the router's
own ID, which is random, so a consumer can tell a live sample from a
replay.

- Values are ordered by stamp. A value older than the one already held
  for its key never replaces it. The mirror, the SDK's `subscribe` and
  the dashboard all apply this, so a replay cannot overwrite a live
  value whichever arrives first.
- A live sample's age is zero. Its stamp is not used for age, because
  a unit on another host would then see every live value aged by the
  difference between the two clocks.
- A replay's age is the time since its stamp
  (`UnitSession.sample_age`).
- The mirror counts age on its monotonic clock from when a value
  arrived, plus how old it was then. A clock step after arrival does
  not change it.

Stamps come from the wall clock. On a host without a hardware clock,
such as a Raspberry Pi, the clock is wrong after a boot until NTP sets
it. Stamps taken in that window are wrong too, and when NTP steps the
clock forward, values from before the step look older than they are.
Order on one host is still consistent, because Zenoh's hybrid logical
clock never goes backwards. Replay ages depend on the wall clock
anyway, since the recorder's rows are wall-clock times.

## Units and manifests

A house is text: one manifest per unit under `units/`, one entity file
per entity in the entities dir of the unit that binds it, an optional
`zones.toml` and an optional `dashboard.toml`. Every file begins with
`schema = 1`. A file declaring another version is refused
(`unsupported-schema`) instead of being half-read. The
field-by-field reference is [`docs/manifest.md`](manifest.md). This
section covers what the files mean and why.

### Unit kinds

Every running thing other than the core is a unit: one OS process, one
manifest, one liveliness token. The manifest's `[unit] kind` is one of
three. It decides which sections the manifest may carry
(`invalid-manifest` otherwise), which key classes the unit may publish,
and where it sorts among ties in the [apply walk](#the-apply-walk).

- An `adapter` puts devices on the bus. It is the only place a device
  dialect is spoken. It must carry `[discovery]` (how it reaches its
  backend) and `[entities]` (where its entity files live).
- An `automation` regulates. It subscribes to state and publishes
  commands. It has no `[discovery]`. It may carry `[entities]` to bind
  entities with no device behind them
  ([Virtual sensors](#virtual-sensors),
  [Commandable virtual entities](#commandable-virtual-entities)).
- A `service` is infrastructure with no entities: the recorder, the
  dashboard, the arbiter, the clock. It may carry `[discovery]` (the
  recorder's store is its endpoint). It may not use `{room}`/`{entity}`
  templates. Only a service may publish under `home/arbiter/`,
  `home/clock/` or `home/history/` ([Reserved classes](#reserved-classes)).

The clock service (`adapters/clock.py`) owns civil time. It publishes
`home/clock/minute` (local RFC3339 with offset) each minute on the
minute, and `home/clock/date` at local midnight. It publishes both at
startup, so a restarted subscriber does not wait up to a minute. Its
timezone is an owner parameter. DST is handled in one place, and no
subscriber does naive time arithmetic.

How many rules one unit hosts is its author's choice
([Unit granularity](#unit-granularity-the-atom-is-the-unit)).
`[runtime]` says how the supervisor runs the unit
([The unit contract](#the-unit-contract)). A manifest declares no
dependencies between units: the bus decouples them, and the one order
that matters is derived from the [grant table](#the-grant-table). It
declares no version, because the repo is the version. It has no health
section, because health is derived from liveliness.

The entity is the resource, and its entity file is the only
authority on its write policy. Automations declare what they want to
publish, not exclusivity. The file stem is the entity's
name. `room` is the single source of truth for where it is. `id` is
the adapter-native address. It is required on an adapter-owned entity
and optional on an automation-owned one. `[write_policy] owner` must
name the unit whose entities dir holds the file (`owner-mismatch`), so
the file states the binding and its location must agree. `[naming]`
(`sv`, `en`, `aliases`) on units and entities supplies labels for the
dashboard and voice.

### The manifest is the contract

The structs in `src/manifest.rs` are the manifest contract.
`deny_unknown_fields` makes a misspelled key an error instead of a
silently ignored line. The structs' doc comments are the field
descriptions. Several things are generated from them:
`homeostat schema [unit|entity|zones|dashboard]` (JSON Schema, also the
MCP `schema` tool), `homeostat schema --markdown`
([`docs/manifest.md`](manifest.md); a test refuses a stale copy), and
the [vocabulary table](#the-capability-vocabulary).

Every rule beyond the shape has a stable error code
(`error[<code>] <subject>: <message> (<file>)`). The `Code` enum in
`src/error.rs` gives each code a paragraph on what the rule is and why.
`homeostat explain`, the MCP `explain` tool and every refused plan read
that paragraph. An error can only be raised with a registered code, and
a test refuses a code that nothing raises. There is one source because
a person or an agent must be able to learn the contract without
reading Rust, and a second description would drift.

### What validation guarantees

`homeostat::check` is the plan-time pipeline every command starts
from. It loads, validates, expands key expressions, and resolves
grants, feeds and sources. Errors accumulate across stages. A file that
fails to parse is reported and skipped, so one bad file does not hide
the errors in the others. `plan`, `apply` and `up` refuse a house with
any error. Warnings refuse nothing. A house that passes guarantees:

- Names are key segments and none is a reserved word
  ([Rooms, entities and zones](#rooms-entities-and-zones)). Unit and
  entity names are unique, and entity ids are unique within one owner.
- Each manifest has the right shape for its kind. Each entity has one
  owner, which exists and holds the file. Capabilities are known, and
  there is a write mode wherever commands go.
- Zone members are rooms that some entity is in, and are not
  pseudo-rooms. Zone names are not room names.
- Each parameter's default matches its type and its own constraint.
- Keys are inside the key space. Templates appear only where they
  expand. State is published only under bound entities. Reserved
  classes are respected and write policy is satisfied
  ([Capabilities, grants and write policy](#capabilities-grants-and-write-policy)).
- References in feeds, sources and `dashboard.toml` resolve.

Validation does not guarantee runtime behaviour. The SDK keeps a unit
inside its declaration, but a process with its own bus session can
publish anything ([Security model](#security-model)).

### Key expansion

`src/expand.rs` expands every `[bus]` expression at plan time by the
rules in [Rooms, entities and zones](#rooms-entities-and-zones).
Templates expand once per bound entity and a zone once per member
room. Cmd and arbiter templates are split by write mode, which is how
the structure enforces [arbitration](#arbitrated-mode). The result is a
derived entity registry that no UI mutates. `plan` prints it
([Plan and apply](#plan-and-apply)), and the SDK performs the same
expansion at runtime against the same files.

An expression that expands to nothing subscribes to nothing while the
unit reports healthy, so plan reports it instead. A template in a unit with no `[entities]` table is an error
(`template-without-entities`). A binding unit with no entities yet, or
a zone with no rooms, is a house being built up, and gets a warning. A
templated `home/cmd/` left empty because every bound entity is
arbitrated is correct and gets no message.

### Discovery

The `discovery` class carries an adapter's complete current view of its
periphery, bound or not. It is one JSON array at
`home/discovery/{unit}`, republished whole each time it changes. Each
record carries:

- `id`, the value an entity file's `id` must use. Only the adapter
  knows its binding rule, so nobody else has to guess.
- whether the device is bound, and by which entity;
- a best-effort `suggested` stanza in homeostat vocabulary;
- the raw protocol descriptor;
- on bound records, the entity's
  [aspect descriptor](#aspect-descriptors).

The format is in [`docs/adapters.md`](adapters.md), section 5.

- One key holds the whole inventory. Device ids may contain `/`, so a
  key segment per device would break. A complete document makes a
  departed device easy to notice, and it matches what the consumer
  does, which is filter on `configured = false`.
- The adapter suggests and the review decides. A hard
  protocol-to-capability mapping would make unknown devices invisible.
  Mapping on the agent side would put a per-protocol table in every
  agent.
- The core mirrors `home/discovery/*` and does nothing else with it.
  Discovery is opt-in. An unbound device is not a fault; it is the
  normal condition of a house being configured.

Out of scope: rooms (no protocol knows them), actuating discovery
(permit-join carries authority) and inventory history (an array does
not fit the recorder's series). The workflow is to read the record,
write entity files for unconfigured devices, run `homeostat plan`, and
have the owner apply.

### The SDK's view of a unit

A unit reads its configuration from the files the core validated. The
supervisor starts it at the house root with `HOMEOSTAT_UNIT` set, and
the unit reads `units/{unit}.toml` and its entities dir. That is why a
manifest's file stem must be its `[unit] name` (`unit-name-mismatch`).
There is no core-to-unit configuration protocol. `[discovery] endpoint`
may reference `${VAR}`, which the adapter expands, because ports and
credentials do not belong in the repo. An unset variable is a startup
error.

Adapters use `homeostat.house.load_adapter` and a `UnitSession`.
Automations use the Context, `homeostat.automation.context()`, which
offers the surface the manifest declares and nothing more:

- `ctx.subscribe`, seeded from the [mirror](#the-last-value-mirror);
- `ctx.params` ([Live parameters](#live-parameters));
- `ctx.publish`;
- `ctx.publish_forecast` ([Forecasts](#forecasts));
- `ctx.restore`
  ([Restoring a unit's own last value](#restoring-a-units-own-last-value));
- `ctx.source_used`
  ([Which sources a computation actually used](#which-sources-a-computation-actually-used));
- `ctx.health_event`;
- `ctx.ready()`, which declares the liveliness token once the unit can
  do its job;
- `ctx.run()`.

`subscribe` and `publish` take a binding name from `[bus.subscribes]`
or `[bus.publishes]` instead of a key. The code says `lights`, and the
manifest says which keys `lights` means. Changing that is a manifest
change, which `plan` shows. `ctx.publish(binding, value, room=,
entity=, aspect=)` puts to one concrete key. Literal segments are
defaults. Wildcard and template segments must be named, because a put
on a `**` expression would hand adapters a key they cannot parse. A key
the expression does not cover is refused. A `home/cmd/` publish is wrapped
in a [cmd envelope](#cmd-envelopes) at the declared
[band](#priority-bands).

## Capabilities, grants and write policy

Authority is declared in text and resolved at plan time. An entity
file says what the entity is (its capability) and how commands toward
it are governed (its write policy). A manifest says what a unit wants
to publish. The plan resolves every publish against the entities into
the grant table. The table is both the permission record the owner
reviews and the dependency graph the apply walk follows.

### The capability vocabulary

A capability says what kind of thing an entity is. The list is fixed in
the core (`CAPABILITIES` in `src/manifest.rs`): `binary_sensor`,
`burner`, `camera`, `climate`, `cover`, `light`, `lock`, `notifier`,
`person`, `presence`, `router`, `sensor`, `switch`. An unknown one is
`unknown-capability`. Each capability has a row in `VOCABULARY`, which
a test keeps in step and which is rendered into
[`docs/manifest.md`](manifest.md). A row gives:

- The **base aspect**, which commands target and the widget acts on
  (`on` for a light, `locked`, a climate's `setpoint`). A capability
  without one takes no commands, and its entities need no write mode.
- Features and reserved aspects: optional aspects an entity file may
  declare (`brightness`), and normalized readings the vocabulary names
  (`indoor_temperature` on `climate`).
- The **notable** reading, which is a deviation on the dashboard's
  `Now` (a light on, a lock unlocked, a router's `wan = false`).

Every capability also has `available` ([Availability](#availability)),
and `{aspect}_valid`, a boolean beside a reading that the device itself
may stop trusting. Some rows are less obvious. `camera` has `motion`,
and its media never goes on the bus ([Cameras](#cameras)). `person` has
`presence`, `lat`, `lon`, `accuracy`, `battery` and `fixed_at`
([Map and people](#map-and-people)). `presence` takes `occupancy` or
`presence`, whichever the protocol uses. `notifier` is described in
[Notifications](#notifications). `cover` is reserved.

The vocabulary is fixed because adapters speak homeostat vocabulary
instead of their dialect's. An automation, a widget, a deviation rule
or a grant written against `light` then works for every adapter that
binds a light. Aspects outside the vocabulary pass through under their
native names, and an [aspect descriptor](#aspect-descriptors) labels
them.

The vocabulary grows by need. The inputs an automation or an interlock
needs belong in the vocabulary, and thresholds are configuration. A
proposal built for one case (a `mode` capability, or a `heat_source`
spanning a burner and a heat pump) waits for a second case.

Presence and connectivity are house state. Network metrics are
observability and belong to the monitoring tooling beside homeostat,
so there is no `vpn` capability. Fusing sightings into "someone is
home" is an automation ([Virtual sensors](#virtual-sensors)). Losing
sight of something does not mean nobody is home. An adapter that loses
part of its view holds those aspects stale and reports it in a health
event.

### Priority bands

Every cmd publish leaves at a band. The band is declared per publish in
the manifest and stamped by the SDK. From lowest to highest the bands
are `automation`, `agent`, `family`, and `manual` (the dashboard and
voice). No unit currently publishes at `agent` or `family`. A cmd
publish without a `priority` is a plan error
(`publish-missing-priority`), since the SDK cannot send it.

The family always wins over automations. Arbitration orders commands by
band, and the manual band is exempt from exclusive-write counting. An
automation that declares `manual` plans with a warning.

A unit that republishes a command re-stamps it with its own band. A
"scene" automation that relays a family button press would therefore
demote family intent below any arbiter hold. To avoid this kind of
band laundering, group actions fan out at the manual edge, and house
modes are latches that consumers read rather than relays
([Commandable virtual entities](#commandable-virtual-entities)).

### Write modes

`[write_policy] mode` is one of:

- `shared`: any granted writer may write, and the last write wins.
- `exclusive`: at most one unit is granted onto the entity below the
  manual band (`exclusive-write-conflict`). Grants are counted per unit
  because authority is per process.
- `arbitrated` ([Arbitrated mode](#arbitrated-mode)).

A capability that takes commands must state a mode
(`write-mode-required`), so a light or a lock never inherits a policy
silently. For other capabilities an absent mode reads as `shared` and
governs nothing.

### The grant table

A grant is one publish resolved against the entities it reaches
(`src/grants.rs`). There are two kinds of row:

- **Writer rows** come from a non-adapter's `home/cmd/` publish. The
  publish must name a capability (`publish-missing-capability`). It is
  granted onto every entity of that capability that its key covers,
  with the band and each entity's room, write mode and owner. A publish
  that matches nothing is a warning.
- **Binding rows** come from a binding unit's `home/state/` or
  `home/forecast/` publish over its own entities.

Adapters' cmd subscriptions form no rows. Adapters embody entities
rather than command them. Compromising an adapter compromises its bound
entities, and no grant could narrow that. Every
bound entity sits in its owner's binding row, and keys are part of a
row's identity. So moving an entity, flipping its write mode, changing
its capability, rebinding it, or widening a publish from `.../on` to
`.../**` is a grant delta, which makes a plan [structural](#tiers).

The table is the permission record. `plan` prints it: the whole table
offline, or the delta against a live world. The supervisor serves the
applied table at `home/meta/system/grants`. The dashboard reads it
there to show which units drive which entities at which band. The
arbiter reads it to know which entities it arbitrates.

The table is also the dependency graph. An entity's owner runs before
the units granted onto it ([apply walk](#the-apply-walk)). Owners can
be automations (latches), so a cycle is possible. A cycle is refused
(`grant-cycle`).

Enforcement happens at plan time and otherwise relies on trust. The
exception is key expansion, which denies adapters a direct path to
arbitrated entities. The hardening step is a bus credential per unit,
which the declarations already shape ([Security model](#security-model)).
Command payloads have no separate validation layer. Adapters check type
and bounds (`invalid-command`).

### Reserved classes

The SDK only checks that a key is inside a declared expression. So the
plan decides which classes a unit may declare at all
(`reserved-class-publish`). Without that check an automation could
declare `home/arbiter/**` and forge post-arbitration commands.

- `home/config/**` and `home/meta/**` belong to the core.
- `home/health/{unit}/...`, `home/discovery/{unit}` and
  `home/hold/{unit}` sit under the publisher's own name.
- `home/arbiter/`, `home/clock/` and `home/history/` each belong to one
  service.
- A `home/state/` publish must name an entity the unit binds
  (`state-publish-unbound`). A `home/forecast/` publish must name an
  entity that exists ([Forecasts](#forecasts)).

### Cmd envelopes

Every payload on `home/cmd/**` and `home/arbiter/**` is an envelope:

```json
{"value": true, "priority": "automation", "actor": "evening_lights", "id": "9f2c1a07"}
```

- `value` is the command. `priority` is the [band](#priority-bands),
  which the SDK stamps from the manifest.
- `actor` is the publishing unit. The recorder stores it with every
  command for the audit trail.
- `id` is minted per command by the SDK. A command is a proposal that
  may be refused or dropped, and only a device readback shows that it
  took effect. The events that end a command (`refuse`, and `drop` with
  `invalid-command`) echo the id as `cmd_id`, so a publisher can tell
  which of two overlapping commands ended. The id is optional. Events
  for a hand-rolled publisher's command report `null`.

Adapters drop a payload that is not an envelope with `invalid-command`.
Both classes travel with blocking congestion control at high priority
([Bus payload conventions](#bus-payload-conventions)).

### Arbitrated mode

High-stakes entities (locks, the heat pump, the burner) are
`arbitrated`. The arbiter service (`adapters/arbiter.py`) holds the
write token for each, and the bus structure prevents anything from
skipping it:

- Writers do not need to know about it. Every writer publishes to
  `home/cmd/{room}/{entity}/{aspect}`. The arbiter subscribes to
  `home/cmd/**` and ignores non-arbitrated entities. It forwards a
  granted wish, with the envelope unchanged, to the same path under
  `home/arbiter/`.
- The arbiter learns which entities are arbitrated from the grant table
  at `home/meta/system/grants`, and follows it live. Flipping an
  entity's write mode restarts the entity's adapter but not the
  arbiter, whose own files are unchanged. A wish for an entity that no
  unit binds reaches no adapter either, so the arbiter drops it with an
  `unbound` event.
- Adapters cannot hear a wish. Their templated `home/cmd/`
  subscriptions expand only over non-arbitrated entities
  ([Key expansion](#key-expansion)). An arbitrated entity that no
  `home/arbiter/` publish covers could never be commanded, so it is an
  error (`arbitrated-uncovered`).
- The arbiter's output is its own class, so no subscription confuses a
  wish with a grant.

The token is a **lease** per (entity, aspect). One entity can carry
independent control dimensions: the family sets a heat pump's
`setpoint` while an automation drives its outdoor offset. A wish is
forwarded when no lease is in force or when it is at or above the
holder's band. It then takes or refreshes the lease for `hold_minutes`,
a family-editable arbiter parameter. A takeover from a strictly lower
band publishes `preempt`. A wish below the holder's band gets a
`refuse` that names the holder and echoes `cmd_id`. When a lease
expires, automations can command the aspect again, so a forgotten
override ends on its own. Events land at
`home/health/{arbiter unit}/event`.

Holds are also published as state. A browser that opens mid-hold needs
to know whether the aspect is held now, and events do not answer that.
The state is one document at `home/hold/{unit}`, mirrored by the core:
`{schema, holds: [{room, entity, aspect, priority, actor, since,
until, refused}]}`. `refused` counts the wishes the hold has turned
away.

- It is one document rather than a key per lease. A restart publishes
  an empty list with no keys to clear, expiry needs one timer, and a
  reader sees a consistent set.
- Enforcement runs on the monotonic clock, and `until` is its
  wall-clock equivalent. A clock step cannot shorten or stretch a real
  hold.
- Expiry is published. Arbitration expires leases lazily, on the next
  wish, so a thread waits on the earliest deadline and republishes.
  Readers also drop an entry past its `until`.

There is no way to hand control back early. An equal or higher band
refreshes the lease, and the only exit is expiry
([Open questions](#open-questions)). Automation-owned entities cannot
be arbitrated (`virtual-entity-arbitrated`). A
[fed input](#device-feeds) never goes through the arbiter: it has one
master, so there is nothing to arbitrate.

### Aspect descriptors

A parameter reaches the dashboard with a type, a constraint and an
`editable_by`, which is enough to build a good control. A raw aspect
has only a name and a value. So an adapter may describe an entity's
aspects in the parameter vocabulary. The description is a
`{schema, groups, fields}` document. Each field carries a label, a
formatting `kind`, a group, and optionally `valid` and `notable`. A
commandable aspect also carries `command: {type, constraint, step?,
editable_by}`, which copies the parameter fields. Firmware names get
labels; they do not become schema. The contract is in
[`docs/adapters.md`](adapters.md), section 5.

- The descriptor travels in the discovery record, as the `aspects`
  member of a bound record. That record is already declared, mirrored
  and per entity. `home/meta/` would not work: it is core-owned and
  served by the supervisor, so a unit's publish there is invisible to a
  fresh reader. A new class would be a second self-description beside
  what discovery already serves. The core knows nothing of descriptors.
- The dashboard still owns every widget ([Dashboard](#dashboard)).
  Adapters never ship UI. An undescribed aspect is moved to a collapsed
  diagnostics group, but it is not hidden.
- Commands are admitted by the same rule that gates parameters. The
  dashboard admits a command that the descriptor declares
  family-editable, within its constraint, once the grant table admits
  the capability. The adapter's bounds remain the enforcement. A knob
  that an automation drives is owner tuning and is not offered to the
  family.
- `notable` makes a described boolean reading a deviation when it is
  true. `readback_s` says how long the device takes to report a command
  back, which only the adapter knows.
- A capability's own aspects (`on`, `locked`) are described as
  readings only. Their controls stay the dashboard's.

### Commandable virtual entities

House modes (day/night, "asleep") have no device. The dashboard,
buttons and the clock flip them, and many lights read them. They are
ordinary automation-bound entities with the shape of a **latch**. The
owner subscribes to `home/cmd/{room}/{entity}/**` over its own
entities. It sets its own state when commanded and does not republish
the command. Consumers read that state at their own bands.

- The capability is `switch`. The room is `global` or a non-spatial
  group name.
- Commandable means the owner listens. A cmd grant onto an
  automation-owned entity needs a covering `home/cmd/` subscription by
  the owner (`virtual-entity-commanded`).
- The write mode is `shared` or `exclusive`, never `arbitrated`. A
  button press travels at the automation band but is family intent, so
  arbitration would rank it below the dashboard. With nothing to
  contend for, last write wins.
- A latch survives its own restart. It adopts its value from the
  mirror, or from the recorder after a core restart
  ([Restoring a unit's own last value](#restoring-a-units-own-last-value)).
  The value is the family's decision, and nothing can recompute it.

Rejected alternatives: an adapter holding modes in memory (which is an
automation dressed as an adapter), a `mode` capability, and a relay
(which launders the band).

### Burners and interlocks

`burner` is a small capability for a combustion heat source. Its base
aspect is `on` (the family lever). Its feature is `power_level`, an
enum constraint that the existing control renders. Its readings are
`flue_temperature` and `boiler_temperature`, in °C. Everything else
passes through raw.

- It is not modelled as `switch` plus sensors. "Is it making heat"
  would then be read from dialect fields. Per-aspect leases also need
  `on` and `power_level` on one entity, so that holding the burner off
  does not freeze an automation's power level. `power_level` is in the
  vocabulary for safety too, since a flue cutout's threshold depends on
  it.
- `on` reads back the device's run state rather than echoing the
  command, because start and stop may be momentary writes. There is no
  run-phase vocabulary until a second burner adapter shows what
  generalises.

Interlocks stay the device's job. Homeostat is not in the safety path,
because no band fits an interlock. At `automation`, an interlock shares
the band of the loop it guards against and is taken over within a
minute. At `manual`, it works but attributes a cutout to the family in
every audit surface. A hold is also timed, while an interlock is
conditional. Refreshing the hold on each sample makes a continuous
writer, and if that writer dies the burner is released without notice.
A house-local cutout at the automation band is welcome as a second
layer, but the house must not rely on it.

- Rejected: a band above `manual`. Whether safety outranks the family
  is a values decision that a constant cannot settle, and such a band
  would still have the timed hold.
- Deferred: an inhibit class, where a unit asserts a lockout on
  `(entity, aspect)` while a condition holds and every band is refused.
  It has the right shape, since an interlock removes an option instead
  of winning an argument. It is the design to pick up if interlocks
  come up again.

## Plan and apply

There is no state file. Desired state is the repo. Actual state is what
the running supervisor reports over the bus. `homeostat plan` diffs one
against the other, and `homeostat apply` commands the supervisor to
make the world match, so there is no state file to drift from
reality. Rollback is git: check out the previous commit,
then plan and apply forward. Plan never reads arbitrary commits.

`plan --bus <endpoint>` (or `HOMEOSTAT_BUS`) reads the core's
queryables as an ordinary client. It reads each unit's applied manifest
and hashes, the grant table and applied commit under `home/meta/`
([Supervision](#supervision)), and the live values under
`home/config/*/*`. With no endpoint, plan runs offline against an
empty world and says so. Every unit is a create and the whole grant
table is printed, which is what a house repo's CI wants. An endpoint
that does not answer is a hard error. Treating it as an empty world
would plan "create everything" against a house that is only
unreachable, and start the home twice.

Plan prints:

- units to create (command, bound entities, parameters), destroy and
  restart (with the reason);
- parameter changes
  (`~ evening_lights/off_time  live="21:30"  repo="23:00"`);
- manifest refreshes;
- the expanded keys of every created unit and of every unit restarting
  on a manifest change, since that new key surface is what an approval
  must show;
- grant changes (the whole table offline);
- feeds and sources;
- warnings, and the tier.

There is no per-subscriber match-set diff. An entity move shows as
grant changes, not as a list of subscriptions that now match
differently.

### Tiers

Every plan has a tier. It is derived from the diff, not declared
(`derive_tier` in `src/plan.rs`):

- **Structural**: a unit is created or destroyed, or the grant table
  changes. Grant changes include entity moves, rebindings, capability
  changes and write-mode flips ([The grant table](#the-grant-table)).
- **Behavioral**: otherwise, a unit restarts because its code, manifest
  or files changed.
- **Parameter-only**: otherwise. Live values reset to the repo and
  parameter-level manifest changes refresh in place. Nothing restarts.

Because the tier is a function of the diff, a structural change cannot
pass as a parameter edit. In the core the tier only gates the apply
lock, which a parameter-only apply skips. Beyond that it is a cue for
the reviewer, and a [pending plan](#pending-plans) is how a structural
plan waits for the owner. The core does not check who sends an apply
([Local-only access](#local-only-access)), and `apply` does not prompt.
Review happens at `plan`.

### Change detection

A unit is unchanged when two hashes match the world's:

- `manifest_hash` is the sha256 of the manifest file.
- `files_hash` is a sha256 over the unit's other repo inputs. These are
  every token of its command that resolves to a file under the house
  root, its own entity files, and `zones.toml` when one of its
  expressions expanded through a zone. For `uv run units/foo.py` the
  hash covers the script and its `{script}.py.lock`, so a dependency
  bump in the lock alone is a change. Paths are hashed with the
  content, so a rename is a change. Imports are not followed. A module
  the script imports is an input only if the command names it.

This is why each unit in `adapters/` is a single script, the recorder's
2,000 lines included. Splitting one into modules would let an edit to a
module go unnoticed by `plan`, and the unit would keep running the old
code after `apply`. Allowing it would take a manifest field that names
a unit's other files, and the size of one script has not been reason
enough to add one.

`[unit] watches = "house"` makes every manifest, entity file,
`zones.toml` and `dashboard.toml` an input of the unit. The dashboard
needs this because it is a view over the whole house. An entity bound
to another adapter changes what the dashboard renders without changing
any of its own files. Without the declaration, `apply` would succeed
and leave the page wrong.

A manifest-hash mismatch is classified by meaning. Both manifests are
compared with every parameter's `default`, `constraint` and
`editable_by` stripped. If they are then equal and the files are
unchanged, the change is parameter-level. That is a refresh with no
restart: the rebuilt store enforces the new constraints and the served
meta manifest updates. Any other difference is behavioral, including
adding, removing or retyping a parameter, because a running unit read
its manifest at startup.

Parameter diffs compare each live value with its repo default. The same
rule covers a changed default and live drift. An integer default on a
`float` parameter is canonicalized. Otherwise `5` against `5.0` would
plan as permanent drift.

### The apply walk

The supervisor executes apply. The CLI sends HEAD to the core's
`home/meta/system/apply` queryable. The supervisor re-validates the
repo and derives its own diff, so the CLI's plan is only a preview. The
supervisor owns the process table, breakers and health, so
restarting a unit and awaiting its readiness works with supervision
instead of racing it. Two alternatives were rejected. A walk driven by
the CLI would need remote per-unit stop and start and its own lock. A
signal telling the supervisor to re-read the repo would give no
verification and no result.

Only one apply runs at a time. A second request while one runs is
refused rather than queued. A parameter-only apply bypasses the lock,
so a setpoint commit never waits behind a structural walk. The walk
follows grant order. Ties and unconnected units are ordered by kind
(adapter, automation, service) and then by name, so the walk is
deterministic:

1. Parameters. The store is rebuilt and every changed value is put,
   under the config write lock. A racing parameter write therefore
   cannot be lost on the bus while the store keeps it.
2. Refreshes of parameter-level manifest changes are recorded.
3. Removals, in reverse grant order, so dependents stop first.
4. Creates and restarts, owners first. Each unit is stopped, launched
   as a fresh supervision task and awaited. Success is health
   `running`. Failure is breaker `open`, `stopped`, or a 60 s timeout.
   A fresh task has a fresh breaker, so new code gets a new failure
   budget.

Apply is per unit and rolling. It is not transactional. A failure halts
the walk in place. The reply names each step's result, the unit the
walk halted at and the units it did not reach, and the CLI exits 1.
Earlier units keep their new incarnations. Neither the served grant
table nor `applied_commit` advances, so a re-run plans the remaining
work. A supervisor that shuts down mid-walk halts it the same way.
There is no automatic rollback. Undoing would be a second walk that can
fail in the same way, and git already gives a forward path back.

`applied_commit` is HEAD, with a `-dirty` suffix when there are
uncommitted changes outside `plans/`. It is published after a fully
applied walk. It is recorded only when the house root is a worktree's
top level, so a fixture nested in another repo does not inherit that
repo's HEAD. At `homeostat up` the repo on disk is taken as applied,
and `applied_commit` stays unset until the first apply.

### Pending plans

`homeostat plan --save` writes `plans/pending/{id}.plan`. The file is
TOML with `id`, `actor` (`--actor`, default `owner`), `created`,
`base_commit`, `tier`, and the rendered plan as a literal string that
is readable on a phone. Saving needs the live world and a git worktree,
and refuses when there is nothing to save.

`homeostat apply --plan <file>` refuses when `base_commit` is not the
current HEAD, `-dirty` included. A pending plan therefore invalidates
itself when the repo moves past it. Otherwise apply recomputes the plan
fresh, as a plain `apply` does. The file is for review; it is not an
execution script. `plans/` does not count toward `-dirty`, and applying
a plan does not remove the file.

## Supervision

`homeostat up` validates the house, opens the bus as a router and
brings up the core's queryables (parameters, health, meta, mirrors,
apply) before any unit spawns, so a unit's first read finds them. Then
one supervision task per unit (`src/supervisor/`) owns the unit's
process, watches its liveliness token, applies its restart policy and
publishes its health. The supervisor also publishes each applied
unit's `manifest_hash`, `files_hash` and `manifest`, and the grant
table. It serves them, with `applied_commit` and `about`, from
`home/meta/**`. This is the live world that `plan` diffs against
([Change detection](#change-detection)).

### The unit contract

The supervisor guarantees the following at spawn
(`src/supervisor/process.rs`):

- There is no shell. `runtime.command` is split on whitespace and
  exec'd directly. `PATH` lookup and relative paths are resolved
  against the house root, which is the unit's working directory. Stdin
  is `/dev/null`.
- The unit gets its own process group, and on Linux
  `PR_SET_PDEATHSIG(SIGKILL)`, so a supervisor killed with SIGKILL
  leaves no unit running.
- The environment is filtered. It contains `HOMEOSTAT_UNIT` (the
  unit's name) and `HOMEOSTAT_BUS` (the endpoint, e.g.
  `tcp/127.0.0.1:7447`). It contains a fixed base set from the
  supervisor's environment: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`,
  `LANG`, `LANGUAGE`, `TZ`, `TMPDIR`, `TERM`, `REQUESTS_CA_BUNDLE`, and
  anything prefixed `LC_`, `XDG_`, `UV_`, `PYTHON` or `SSL_CERT_`. It
  also contains the variables the manifest names in `runtime.env`, by
  exact name. Nothing else is passed, so a secret handed to the
  supervisor for one unit never reaches another.
- Output is captured. It is re-emitted on the supervisor's streams
  with the prefix `[{unit}] `, and kept in a 500-line ring at
  `home/meta/{unit}/log`
  ([Logs and the audit trail](#logs-and-the-audit-trail)).

In return, every unit must:

- connect as a client to `HOMEOSTAT_BUS` with scouting off. Zenoh peers
  do not route between clients, so the supervisor's router is the hub.
- declare a liveliness token at `home/health/{unit}/alive` once it can
  do its job (`UnitSession.ready()`). The token, rather than the PID,
  means "up". It disappears with the session, whatever the process
  does.
- exit on SIGTERM within `runtime.shutdown_grace_s` (default 5 s).

### Resolving `uv run`

For a command `uv run [flags] <script.py> [args]`, the supervisor runs
`uv sync --script` and `uv python find --script` to completion. It then
execs the environment's interpreter on the script, so the interpreter
is the group leader and holds `PR_SET_PDEATHSIG` directly. Any other
command is left alone.

- Without this, `uv run` stays alive as an idle parent. It holds 4 MB
  when warm and 25-55 MB after a fresh resolve. It also keeps the
  interpreter out of reach of `PR_SET_PDEATHSIG`, which reaches only
  the direct child.
- The manifest still says `uv run`. The PEP 723 block stays the single
  authority on dependencies, and `files_hash` covers it. Every start
  resolves again, so a restart after an SDK bump picks the bump up.
- This is best effort. If uv cannot resolve the script (no PEP 723
  block, a broken dependency, no network), or the interpreter path
  would not survive whitespace tokenizing, the original command runs
  unchanged and fails as `uv run` would.

### Health

The supervisor publishes JSON at `home/health/{unit}` on every
transition and serves it from a queryable at the same key. Nothing
republishes it.

```json
{"status": "backoff", "pid": null, "restarts": 2, "backoff_ms": 400, "last_exit_code": 1}
```

The statuses are:

- `starting`: spawned, with the token not yet seen, or lost while the
  process lives;
- `running`: token present;
- `backoff`: exited, with a restart due in `backoff_ms`, which is
  present only in this status;
- `open`: breaker open, no more restarts;
- `stopped`: policy `never`, a clean exit under `on-failure`, or
  shutdown.

`restarts` counts from the start of the unit's supervision task, which
is supervisor start or the last apply that restarted the unit. Each
incarnation gets a fresh liveliness subscriber, so a token event left
over from the previous one cannot mark the next one `running` early.

### Restart policy, backoff and the breaker

`runtime.restart` says which exits count: `always`, `on-failure`
(non-zero or a signal) or `never`. Restarts then follow one rule
(`src/supervisor/backoff.rs`). The first restart waits 100 ms, and each
further consecutive quick exit doubles the wait, up to 30 s. A run
lasting 5 s resets the count. The fifth consecutive quick exit opens
the breaker, so the waits are 100, 200, 400 and 800 ms. Any quick exit
counts, clean or not, because a loop of clean exits is as much a crash
loop as a loop of panics. A failed spawn counts as an exit.

An open breaker stays open until the supervisor restarts, or until an
apply restarts the unit with a fresh task and breaker
([The apply walk](#the-apply-walk)).

### Termination and sweeping

- Stopping a unit (at shutdown, or in an apply step) sends SIGTERM to
  its whole process group. The supervisor waits up to the grace period
  for the whole group, not only the leader, and then sends SIGKILL to
  the group.
- When a leader exits on its own, the supervisor kills the rest of its
  group with SIGKILL before restarting. A descendant left behind could
  keep the liveliness token alive into the next incarnation.
- On SIGTERM or SIGINT the supervisor stops every unit in parallel and
  reports each as `stopped`. The handlers are installed before the bus
  opens, so a signal during startup is also handled gracefully.
- Global reaping is done by tini, which is PID 1 in the container
  image.

## Live parameters

A parameter is a value a running unit reads and that someone may change
without a restart: an off time, a hold duration, a poll interval. A
unit declares each one under `[params.<name>]` with a `type` (`bool`,
`int`, `float`, `string`, `time`), a `default`, an optional
`constraint` and an optional `editable_by` (`owner` or `family`). The
current value lives at `home/config/{unit}/{param}`. The clock's
timezone, the arbiter's `hold_minutes` and an adapter's poll interval
use this path like any automation's setpoint.

The constraint language is small: `min`/`max` for numbers,
`after`/`before` for times (`"HH:MM"`, a window that may span
midnight), and `enum` for strings. A parameter that needs more is
`editable_by = "owner"`, so the person changing it can read the code.

### The write path

Only the core puts on `home/config/**`. At startup it seeds every
parameter from its manifest default and declares one queryable on
`home/config/*/*` (`src/config.rs`):

- A GET without payload reads what the selector covers.
- A GET with payload is a write request for one key. The core checks
  the JSON value against the manifest's type and constraint. If the
  value is accepted, it is stored, put on the key (subscribers see it
  at once) and echoed in an ok reply. If it is rejected, the reply
  names the violation, nothing is put, and the old value stands.

A write is a query because one owner, the core, answers it in full
while the query is open. The writer learns synchronously whether the
value is in force. A plain put would bypass validation. The Zenoh
storage plugin is not used because a passive mirror cannot refuse an
out-of-constraint write
([The last-value mirror](#the-last-value-mirror)).

A write and its put are one ordered step under the same lock that
apply's parameter step takes ([The apply walk](#the-apply-walk)), so
the store and the bus never disagree. An integer written to a `float`
parameter is stored as the float it means, in the same way the repo
default is read. The two never differ by representation alone.

### Repo and live values

The repo is the system of record, and the bus value is a live view of
it.

- A live edit survives a unit restart, because the core holds it. It
  does not survive a supervisor restart, because the store re-seeds
  from defaults.
- Plan shows drift and apply resets it. Every live value that differs
  from its default is listed, and every apply sets live values to the
  repo's ([Change detection](#change-detection)).
- To make a live edit durable, commit it as the default. That is a
  parameter-only plan, with no restart and no apply lock.
- Changing a constraint or `editable_by` refreshes the unit in place.
  Adding, removing or retyping a parameter restarts it.

A default outside its own constraint is `invalid-default`, so the repo
path is checked as strictly as the live one.

### Who may write

`editable_by` says who may change a parameter live. The dashboard
offers `family` parameters as setpoints, shows `owner` ones read-only,
and writes nothing else. The core's queryable does not know who is
asking, so any bus client may write any parameter within its
constraint. The boundary is access to the bus
([Security model](#security-model), [Open questions](#open-questions)).

### In the SDK

A unit follows its own `home/config/{unit}/*` without asking, by
subscribe, then get, then merge
([The last-value mirror](#the-last-value-mirror)).

- Automations read `ctx.params.<name>`, the typed current value
  (`time` as `datetime.time`). An undeclared name raises.
- Adapters and services use `LiveParams` (`homeostat.params`). It has
  defaults of its own, so a manifest may omit a parameter. It tracks
  finite numeric values only.
- Writers (the dashboard) call `UnitSession.write_config`. It raises
  `ConfigWriteError` with the core's message when the core refuses.

## State, history and forecasts

Three stores answer three questions. The core's last-value mirror says
what is true now. The recorder says what was true and what was
predicted. Forecasts say what a source claims will be true. All three
are read over the bus, so no consumer holds a database handle or a
credential, and each store stays private to its owner.

### The last-value mirror

The core keeps an in-memory last-value cache in the supervisor process
(`src/supervisor/mirror.rs`). For each mirrored key space it declares a
subscriber and a queryable on the same expression. A put replaces the
key's entry unless the entry has a newer stamp
([Timestamps](#timestamps)), and a delete removes it. A GET replies
once per matching key with the last payload, byte for byte, and with
its stamp. The mirrored
spaces are `home/state/**`, `home/forecast/**`, `home/clock/*`,
`home/discovery/*` and `home/hold/*`. `home/config/*/*` and
`home/health/*` have last-value queryables of their own
([Live parameters](#live-parameters), [Supervision](#supervision)).
All are up before any unit spawns.

- Every reply carries the value's age in its attachment. The age counts
  on the mirror's monotonic clock from when the value arrived, plus how
  old it was then: zero for a live sample, the time since its stamp for
  a replay (`get_json_aged`; see [Timestamps](#timestamps)).
- The read pattern everywhere is subscribe, then get, then merge.
  `ctx.subscribe` does this for every binding. It delivers each key's
  catch-up value, with its age, before any live sample for that key
  ([Staleness](#staleness)).
- The mirror does not inspect payloads and expires nothing. It cannot
  know a producer's cadence, so whether a value is too old is for the
  consumer to decide ([Availability](#availability)).
- It is not durable. A core restart empties it, and every upgrade is a
  core restart. The core then replays the recorder's last value of
  every state series
  ([Replay after a core restart](#replay-after-a-core-restart)).

### History and the recorder

The recorder (`adapters/recorder.py`) is a generic service unit, one
per house ([Reserved classes](#reserved-classes)). It writes one SQLite
file named by its `[discovery]` endpoint (`sqlite:<path>`, relative to
the house root, with `${VAR}` expanded). It is not a bus mirror.
Payloads are decoded and typed on the way in. Anything that fails
leaves a `drop` health event instead of a row of garbage.

The recorder uses SQLite in production and in tests. A home produces
well under ten samples a second, which an indexed SQLite file can
absorb for years. CI runs the same engine with nothing beyond
`cargo test`. Every read goes over the bus, so outgrowing SQLite would
change one unit. Rejected alternatives:

- QuestDB or TimescaleDB would need a permanent JVM or Postgres
  cluster outside the unit model.
- DuckDB is columnar, weak at single-row inserts, and single-process.
  It can instead `ATTACH` the store or an archive read-only for
  analysis.
- A pluggable backend would let tests and production diverge.

Tiering [archives](#archives) to Parquet is held in reserve. It would
make them about 18x smaller at the cost of a ~50 MB dependency and a
second read engine.

The recorder records whatever its manifest subscribes. The shipped
manifest takes `home/state/**` and `home/cmd/**` into `samples` (a
command as its envelope's `value`) and `home/forecast/**` into
`forecasts`. Into `events` it takes the raw payloads of
`home/health/**`, `home/config/**` (only accepted writes are put there)
and every command envelope
([Logs and the audit trail](#logs-and-the-audit-trail)). It does not
record `home/clock/**` (a derivable row every minute, forever),
`home/meta/**`, `home/discovery/*`, `home/hold/*`, liveliness tokens or
`home/history/**`. The schema is defined by `init_store()`, versioned
by `PRAGMA user_version` and migrated in place.

- Series identity is `(class, entity, aspect, source)`. The room is a
  tag on each row, so an entity that moves stays one continuous series.
  `source` is `''` rather than NULL except for forecasts, because SQLite
  treats NULLs as distinct in a unique index. Names are interned, which
  keeps a `WITHOUT ROWID` sample row near 25 bytes instead of about 113
  as text.
- Each series carries its own tally (rows, oldest, newest). Without it,
  `stats` would scan a store that grows with the problem it diagnoses,
  and `ctx.restore`, which polls `stats`, would time out.
- A live sample's timestamp is the recorder's receive time (µs, UTC),
  assigned before any buffering, so an outage does not distort history.
  A sample whose publisher set its own stamp, which today is only the
  core's replay, is recorded at that stamp instead. A state sample is
  then skipped when the series already has a row at or after the stamp,
  within half a second, so a replay never duplicates the row it came
  from. Two samples for one series in the same microsecond collide, and
  the later one is dropped.
- Repeats are kept. A republished value is still a sighting. Dropping
  repeats would help with a device that republishes, but not with a
  jittering float or an honest 1 Hz sensor. [Retention](#retention) and
  [archives](#archives) bound the volume, and reads collapse repeats.
- Only scalars are stored
  ([Bus payload conventions](#bus-payload-conventions)). Anything else
  is dropped (`non-scalar`, `non-finite`).
- `auto_vacuum = INCREMENTAL` lets retention return pages. It can only
  be set before the first page is written. WAL keeps the writer and
  readers from blocking each other.

Subscriber callbacks stamp, type and enqueue. One writer thread commits
one transaction per flush, on a connection opened for that flush, so
no long-lived handle holds stale permissions or a deleted inode. Reads
open their own read-only connections. The store must open before
`ready()`, so a recorder without a store shows as backoff.

- On an outage (disk full, permissions, a dying SD card), a failed
  flush keeps the batch in a 10,000-row buffer that drops the oldest
  rows first, since recent state is worth more. The flush is retried on
  new samples and every second. There is one `backend-outage` event
  per transition to down. On recovery the rows land with their original
  stamps, and `backend-restored` reports what was flushed and what was
  dropped.
- A poison row is bad data, not an outage. The batch is replayed one
  statement at a time. The refused row leaves a `drop`
  (`integrity-error`) and the rest commits.
- SQLite has no page checksums by default, so a separate thread runs
  `PRAGMA integrity_check` read-only every `integrity_check_hours`
  (owner parameter, default 24, 0 disables). The first run comes one
  interval after start, so a restart loop does not hammer a large file.
  It reports `integrity-ok` or `integrity-failed` for the owner to act
  on.

A recorder that subscribes at start misses what was published just
before, so a rarely-changing aspect could look as if it had never been
published. After subscribing, the recorder therefore catches up from
the mirror. It enqueues the mirror's `home/state/**` values that it has
not seen live, stamped at the value's own time (now minus its age). It
skips a series that has a row at or after that time, within half a
second. A start order that puts the recorder first would not help: it
misses a recorder restart, and it is a dependency edge between units.
While the recorder is down there is a gap, and nothing replays it.

### Read path

The recorder serves one queryable at `home/history/**`. It is declared
under the recorder's `[bus.publishes]`, so the plan shows the read
surface. Parameters use Zenoh's `;` separator with no URL decoding, so
the `+` of an offset stays literal. Keys are entity-first with no room
slot.

- Samples:
  `home/history/{state|cmd}/{entity}/{aspect}?from=..;to=..;limit=..`
  (RFC3339 with offset). There is one reply per matching series, an
  array of `{ts, room, value}`. Two mutually exclusive folds apply to
  the whole window before `limit`. `bucket=<seconds>` gives a number's
  mean with `min` and `max`, or otherwise the last value; more than
  10,000 buckets is refused. `changes=1` gives the rows where the value
  changed. Without folds, a chart's span would depend on the publish
  rate. The page knows its width and the recorder knows the rows, so
  the recorder does the downsampling.
- Forecasts: `home/history/forecast/{entity}/{aspect}/{source}`,
  returned as issues in the wire's shape. `at=` (default now) returns
  the latest issue at or before that instant.
  `valid_from=..;valid_to=..` returns every issue that spoke about the
  window, with only its overlapping points. Verification reads this.
  `limit` counts issues.
- Events: `home/history/events?key=..;from=..;to=..;limit=..`. `key`
  is a key expression and the bounds are integer microseconds.
- Stats: `home/history/stats` gives the size and span of the store,
  of each series, of the events table and of each archive. It reads the
  tallies and does not scan. Choosing a retention window means knowing
  which series fills the file, and a host may lack `sqlite3`.
- Latest: `home/history/latest` gives every state series' newest row as
  one array of `{key, value, ts}`, with the state key it was recorded
  under and `ts` in integer µs. The core reads it to
  [replay after a restart](#replay-after-a-core-restart). It reads the
  hot file only: each series' newest row stays there when its month is
  archived.

Every path clamps `limit` to 10,000, keeps the newest rows, and replies
oldest first. Anything malformed or unexpected gets an error reply, and
an unexpected failure also emits a `query-failed` event. A callback
that raises sends no reply, which looks the same as "nothing recorded".
A history API must not give that wrong answer. Zenoh runs a
queryable's callback serially, so a slow answer delays every query
behind it.

### Retention

There are three owner windows, in days: `retain_samples_days`,
`retain_forecasts_days` and `retain_events_days`. Events are the audit
trail and are worth keeping longest. Each defaults to 0, meaning
forever, so no upgrade silently deletes history. Forecasts are purged
by issue time, since superseded issues are what grows. The writer
thread purges hourly and when a window changes. It deletes per series
in short primary-key ranges, then runs `PRAGMA incremental_vacuum`.
Each purge that deleted anything emits one `purge` event; an empty
purge is silent. After `purge-failed` the purge is retried an hour
later.

Retention is the only operation that deletes from the store. It also
makes noise visible. An adapter that publishes on every poll instead of
on change fills the file, and `stats` names the series. There is no
downsampling. A roll-up that deleted its source rows could not be
additive, and an analytical layer over `ATTACH` can roll up without
deleting.

### Archives

Archiving moves rows out of the store without deleting them, so the
store stays one window deep and every observation is kept. When
`archive_after_months` is above 0 (owner parameter, default 0, meaning
never), each month that closed longer ago than that is sealed into
`archive/<store>-YYYY-MM.db`. One month is sealed per pass, on the
writer thread after the hourly purge, so a row past its window is
deleted rather than archived. The events are `archive` and
`archive-failed`. `home/history/**` answers from the store alone. Every
query the system makes lies within a month or two, and archives are for
people and tools.

- An archive is a plain, uncompressed SQLite file with the store's
  schema and ids, readable the same way as the store. An archive that
  must be unpacked first is one nobody opens.
- An archive is sealed once and not written again. It is written under
  a temporary name, verified, fsynced, renamed, recorded with its
  SHA-256 and made read-only. A crash mid-seal loses nothing, since
  nothing was pruned yet. The next pass seals a file that is present
  and discards an attempt whose file is missing. Late rows go into
  `.2`, `.3`. The checksum is all a later check needs, and a backup's
  diff is only the current window.
- Only what a sealed file holds leaves the store, matched on the whole
  row. Each series' newest sample and newest forecast issue stay in the
  store until overtaken, because `ctx.restore`, the seed and every
  latest-value read look in the store.
- `retain_archives_months` (owner parameter, default 0, meaning
  forever) drops whole files. It is a separate setting so that a store
  window never deletes archives. It deletes the file before its record,
  so a crash leaves work that the next pass finishes
  (`archive-dropped`).
- Settings that undercut each other are allowed and are reported once
  per change as `archive-misconfigured`. Examples are a `retain_*_days`
  window shorter than `archive_after_months` + 1 months, or archives
  kept for less time than archiving waits.

### Forecasts

Homeostat assumes model-predictive control, which needs forecasts. It
provides only the mechanism: a place to put a forecast, a
store that keeps every issue, and a chart that draws them. Producers
and controllers are house behaviour ([Repo split](#repo-split)). A
forecast is **a source's claim about a series' future values**. A
controller's planned trajectory is such a claim, so it needs no second
class. Whether a claim is a prediction or an intent follows from the
aspect ([Sources](#sources)).

- A forecast is keyed like the series it extends, plus who says so
  (`home/forecast/{room}/{entity}/{aspect}/{source}`). It shares the
  series' descriptor and chart axis. Several sources may speak about
  one aspect. The source slot is required. With two key shapes, no
  single wildcard expression would match every opinion about a series.
- The entity must exist, but the publisher need not bind it
  (`forecast-publish-unbound`). A weather service or a controller is
  usually not the binder. Two units whose publishes can land on the same
  key are a `forecast-publish-conflict`, compared slot by slot, with a
  wildcard colliding with any literal. The check exists because the
  mirror keeps one document per key.
- A forecast differs from state by having two time coordinates: when
  it was said and when it is about. So even a one-point forecast cannot
  go into `samples`, where two issues about one instant would collide.
  The payload is one issue, published atomically:
  `{schema: 1, issued, points: [{t, v, d?}]}`. Timestamps carry an
  offset, because a forecast that is an hour off is worse than none.
  - Points are irregular, as the source gave them, because resampling
    has more than one valid answer (a price holds, a temperature
    interpolates). The consumer names the rule.
    `Forecast.at(when, mode, max_gap_s)` takes `step` or `linear` with
    no default. It and `resample()` give `None` across a gap, so a
    controller can refuse instead of optimising against invented data.
  - `d` is a point's extent in seconds, covering `[t, t + d)`. Without
    it an accumulation reads as a spike, and the last held value of a
    horizon has no length. Reading an interval point as `linear` raises.
  - `issued` is required and is all there is to staleness. The consumer
    applies its own maximum age (`Forecast.age_s()`).
  - `put_forecast` refuses a bad payload (over 2048 points, non-finite
    values, duplicate instants) with a `drop` (`invalid-forecast`).
- The core mirrors forecasts. Otherwise a consumer starting at midday
  would not see a day-ahead curve until the next day. Producers publish
  their current forecast at startup, because a core restart empties
  the mirror.
- Forecasts are stored per point, and every issue is kept, since the
  point of recording them is to check superseded issues against the
  outcome. Rows carry the producer's `issued` rather than the receipt
  time, so a replayed issue cannot pose as fresher. An issue is
  accepted or refused whole. `valid_end` is stored, because the last
  window has no successor to derive it from. Widening `samples` instead
  was rejected. A nullable `issued_ts` cannot be part of a
  `WITHOUT ROWID` key, folds would average across issues, and retention
  would need a purge that depends on the class. Rows older than the
  source segment carry the reserved source `_unknown`.
- A quiet producer is not diagnosed from the age of its document. A
  crashed producer is a supervision matter, and a failing upstream is
  reported in the running producer's health event. The
  [dashboard](#dashboard) draws forecasts under its own staleness
  policy.
- Uncertainty and corrections to the past cannot be represented
  ([Open questions](#open-questions)). Publishing percentiles as
  separate sources is the tempting misuse. One issue's percentiles are
  one claim.

### Replay after a core restart

The mirror is in memory, so a core restart empties it, and every
upgrade is a core restart. Without a replay, a value published on change
is missing from the bus until its source publishes again. A fused
indoor temperature, for example, waited for both of its thermometers,
which report every 10 to 30 minutes. The recorder held every one of
those values all along.

When the recorder first reaches `running` after the core starts, the
core reads `home/history/latest` and publishes each entry again on its
state key (`src/supervisor/replay.rs`).

- The stamp is the row's recorded time with the replay ID
  ([Timestamps](#timestamps)). A consumer sees the value with the age it
  really has.
- Only entities bound in the applied grant table, in the room they are
  bound in, are replayed. An entity removed or moved since its value was
  recorded does not come back under its old key.
- A key the mirror already holds is skipped, since a value published
  since the core started is newer. If one arrives after the replay
  instead, its newer stamp wins in the mirror and in every subscriber.
- It runs once per core start. A failed read is logged and not retried,
  and the house then starts with an empty mirror, as it would without a
  recorder.
- Only state is replayed. A forecast is a document rebuilt from rows,
  not one row, and its source publishes again on its own schedule.

The core publishes the replay rather than the recorder. Only an
entity's binding unit publishes its state
([Capabilities, grants and write policy](#capabilities-grants-and-write-policy)),
and a recorder declaring a publish on every state key would break that.
The core is not a unit and already publishes `home/config` and
`home/meta`.

What each consumer does with a replay:

- `subscribe` delivers it with its age, unless a newer value for the
  key was already delivered. A fusion sees an old input with its age
  and decides with `Freshness`, instead of seeing nothing.
- The dashboard shows it until a newer value arrives.
- The recorder skips it, since it holds the row the replay came from.
- `ivt490`'s device feeds ignore it
  ([Device feeds](#device-feeds)). A pump is fed readings, not history.
- A one-way sender still publishes `false` at start
  ([One-way senders](#one-way-senders)). Its live `false` is newer than a
  replayed `true`, so it wins whichever arrives first.

### Restoring a unit's own last value

The mirror survives a unit restart. After a core restart the core
replays recorded state ([Replay after a core restart](#replay-after-a-core-restart)),
but a unit may start before that replay. A latch that started on its
code default would then disagree with what the family set, at least
until the replay arrived. The right behaviour on start depends on the
kind of state, and only the unit knows which kind it holds. So
restoring is a call the unit makes, and the framework does not do it
automatically:

| unit | on start | why |
|---|---|---|
| one-way sender adapter | publish `false` | the held value was its own construct ([One-way senders](#one-way-senders)) |
| fusion | recompute, seeded from the mirror | derived, and a stale input is dangerous |
| latch | restore what was last published | a person decided it; age is irrelevant |

- `ctx.restore(binding, room=, entity=, aspect=, timeout_s=30)` returns
  `(value, age_s)` or `None`. It reads the series' newest row in the
  recorder, which is the only record that a decision was made. It reads
  only the unit's own published state keys, resolved through the
  binding in the same way as `publish`. Somebody else's state is a
  different and worse thing to restore, and a command is an event with
  nothing to restore.
- The age comes with the value. A latch ignores it and a fusion checks
  it. Returning a bare value would make the dangerous case the easy
  one.
- It returns `None` rather than raising when there is no recorder, no
  rows, or a store error (`restore-failed`). A house without a recorder
  then starts on code defaults.
- It waits for the recorder, because there is no start order. It polls
  `home/history/stats` until the timeout, since an empty series gets no
  reply. It does not wait if no unit publishes under `home/history/`.
  Call it before `ready()`. The recorder answers serially, so restore
  waits out a get the recorder has taken instead of adding another to
  its queue. Only a get that was not served is repeated. Reads are not
  a declared bus surface. What constrains `restore` is the unit's own
  publish bindings.

Persisting unit state in the SDK or the supervisor was rejected. It
would be the same framework guess, made without the value's age. A file
beside the unit was also rejected. It would be a second store with its
own retention, backup and corruption problems.

## Derived values

A value the house computes rather than measures (a fused temperature,
"someone is home") is ordinary state on an ordinary entity, bound by
the automation that computes it. Consumers cannot tell whether a value
was measured or derived, in the same way that the bus does not leak
adapter-native vocabulary. Provenance is visible where structure lives:
in the entity file's owner and in the plan.

### Virtual sensors

A virtual sensor is an entity whose binding unit is an automation. One
unit binds each entity, whether it is an adapter or an automation.
Presence fusion (router sightings, phones and motion combined into
"someone is home") is the typical example.

- An automation may carry `[entities]` ([Unit kinds](#unit-kinds)),
  which are expanded as an adapter's are. Their files may omit `id`,
  because a computed value has no device-native address.
- The entity file is what gives the value everything downstream for
  free. The recorder, the dashboard widget, the mirror and
  `read_state`, the notable vocabulary and the voice grammar all come
  from the entity registry. A free-form state key would be recorded but
  invisible to every generated surface. So a state publish must fall
  under an entity the unit binds (`state-publish-unbound`). Forecasts
  only require the entity to exist ([Forecasts](#forecasts)).
- A virtual sensor is read-only unless its owner listens for commands,
  which makes it a latch
  ([Commandable virtual entities](#commandable-virtual-entities)).
  Chains of virtual sensors need no ordering, because a late joiner
  reads the mirror.
- A fusion across rooms lives in `global`. "Downstairs" is a zone, and
  zones never appear in keys. A virtual sensor that is really about one
  room uses that room. Where it appears on the dashboard is up to
  `dashboard.toml`, which does not create a second spatial truth.
- Staleness is the producer's obligation. This is a norm and not
  machinery, since the core cannot know which inputs a fusion needs. A
  fusion of stale inputs goes stale instead of confidently
  republishing ([Staleness](#staleness)). It reports which inputs it
  used ([below](#which-sources-a-computation-actually-used)).

Rejected alternatives:

- A `derived` key class would fragment the vocabulary every consumer
  keys on.
- A generic fusion adapter with rules would not fit, because the choice
  of sensors and weights is house behaviour, and a rule language for it
  is a DSL.
- Fusion across adapters inside one adapter is not allowed. An adapter
  may derive values on its own bound entities and no further.

### Device feeds

A heat pump's "actual indoor temperature" input is not a command. It
is a continuous signal with one master, and the failure that matters is
staleness rather than contention. A **feed** wires a device input to
one source aspect. Which inputs are fed is a per-house decision, so the
wiring lives in the fed entity's file:

```toml
[inputs]
indoor_temperature_actual = { entity = "indoor_temperature", aspect = "temperature" }
```

- The reference names an entity and aspect rather than a bus key or a
  unit. Entities are the identity layer that keys derive from, and a
  device consumes one signal. The plan resolves the reference to a
  state key and prints the edge under `Feeds:`.
- The plan checks (`resolve_feeds` in `src/grants.rs`) that the source
  entity exists (`input-unknown-entity`), that an automation-owned
  source publishes the aspect (`input-unpublished-aspect`), and that
  the fed entity is a device (`virtual-entity-fed`). An aspect of any
  owner can be fed.
- The adapter is the authority on input names, because they are
  dialect knowledge. It refuses to start, visibly, on an input it does
  not know. For an entity with a feed, it drops the corresponding
  command aspect, so the input keeps one master.
- Staleness is handled by the device. The adapter forwards live source
  samples while the source's `available` is not false. A replay after
  a core restart is not forwarded, since its value can be old. After that, the
  device's own validity window expires the term and the device falls
  back on its own control. There is no timeout or refresh in the
  adapter. A source that publishes only on transition and is quieter
  than the window is a matter of house tuning.
- A fed value must not outlive its source in a broker, which would
  serve it across reconnects. It is published unretained. On source
  loss the adapter clears any retained copy and reports
  `feed-source-lost`.
- The adapter uses one subscriber per source entity (`.../{entity}/*`).
  Zenoh orders samples only within a subscriber, and a value that
  overtook the `available = true` before it would be dropped. A value
  that arrives while the source is unavailable leaves one `drop`
  (`feed-source-unavailable`) per outage.
- Feeds are not edges in the walk order. A control loop that reads a
  device and feeds a term back is legitimately cyclic, so the
  [apply walk](#the-apply-walk) orders by grants only.
- One computation may do both, feeding one output and commanding
  another. Nothing guarantees the two land together. An automation that
  needs that holds the command's lease and feeds against it.

Rejected alternatives:

- A fifth command aspect marked non-arbitrated would misdescribe
  sensor feedback and put it in the grant table next to setpoints.
- A new grant kind would be machinery for what an entity-file
  reference already expresses.
- An adapter subscribing to a raw bus key would bypass the identity
  layer.

### Sources

The key space names the thing that reports, not the thing reported on.
For a lamp they are the same object. They come apart when several
sensors and a weather service all have opinions about the outdoor air.

- Whether something is physical or virtual is invisible and does not
  shape a key.
- Whether something is commandable or read-only is a property of an
  aspect, not of an entity. Leases are per aspect, and descriptors
  carry `command` per field ([Aspect descriptors](#aspect-descriptors)).
  A heat pump's `feed_temperature` is a reading, and its
  `feed_temperature_target` takes commands. Asking "sensor or
  control?" per entity is what makes a second noun for the subject seem
  necessary.
- A plan and a prediction are one class, and which one a forecast is
  can be derived. A forecast for a commandable aspect is a plan, and
  one for a read-only aspect is a prediction, so there is no `kind`
  field. Divergence from the outcome measures a prediction's accuracy
  but a plan's authority (it may be revised, arbitrated away or
  clamped). An error metric must therefore not average the two.
- Only forecasts have a source segment. Two outdoor sensors are two
  entities, since a sensor is in the house with a room and a failure
  mode. A weather service is not in the house, and an entity for it
  would put subject and provenance into one name. Competing estimators
  of one state value are separate entities until that stops being rare.

A computed entity may declare what it is computed from, using the same
reference shape as a feed:

```toml
[sources.kitchen]
entity = "kitchen_temp"
aspect = "temperature"
note = "south-facing; reads high on a sunny afternoon"
precision = 0.5
```

- Sources are declared, not inferred. A fusion subscribes to many
  things for many reasons, and no subscription says which inputs feed
  which aspect.
- They are checked like a feed (`resolve_sources`:
  `source-unknown-entity`, `source-unpublished-aspect`). The plan also
  warns about a declared source that the owner does not subscribe to,
  so the declaration is a checked fact. The plan prints the edges under
  `Sources:`.
- `note` and `precision` are the contributor's own caveats, such as
  "it sits in the sun" or "it is coarse but not disagreeing". The
  overlay shows them beside the contributor. Kind and unit stay on the
  aspect descriptor, which is a contract every source is held to.
- `[sources]` is separate from `[inputs]`, though the shape matches. A
  feed carries a runtime contract and retires a command aspect. That
  would collide on a commandable virtual entity that also declares
  sources.
- Nothing in the store changes. Each contributor is its own series. The
  overlay's `sources` view ([Dashboard](#dashboard)) draws each as its
  own line. It does not draw a band, because a band's edge would trace
  a path no sensor took.

A second noun for "the thing reported on", with its own key shape and
a rule for choosing the canonical value, was rejected. The points
above, `[sources]` and the optional `id` on automation-owned entities
cover its cases.

### Which sources a computation actually used

Declared sources say what may contribute. A fusion that drops one as
stale, implausible or excluded knows more. Without that knowledge the
overlay would draw an excluded contributor as participating. But
finding out that "the shed sensor is why this went stale" is the reason
someone opens the overlay.

- There are two health events, emitted on transition:
  `source-dropped` and `source-restored`. They carry `entity`, `aspect`
  and `source` (its `[sources]` name), and they are recorded and read
  back like any event.
- The SDK remembers state, so the producer does not have to. The
  producer calls `ctx.source_used(entity, aspect, source, used)` each
  time it decides, and the SDK emits only on a change. Hand-written
  producers tend to get that wrong.
- A source with nothing on record is participating, as declared. The
  first call per source always reports, so a consumer starting
  mid-window does not read silence as agreement. That report is best
  effort. Health events are not mirrored, so one published before the
  recorder subscribes is lost. If that misleads a house, the remedy is
  to mirror participation or make it queryable on the producer.
- The overlay reads events from a week before its window, so a source
  excluded earlier is not shown as live for the whole span. It marks
  exclusion in the legend and keeps drawing the line, because the point
  where the line stops is the diagnosis.

Participation is not ordinary state. A list is not one scalar. A
boolean aspect per source would put the source's name into the
aspect's name. A count cannot say which source.

## Health, availability and the audit trail

Three kinds of failure need three signals. The supervisor reports a
unit that dies ([Supervision](#supervision)). A unit reports, as state,
a device that dies behind it while the unit stays up. Something a unit
refused, dropped or noticed is a health event. All three land on the
bus, the recorder keeps the last two, and logs stay outside the trail.

### Health events

A unit reports on itself in two ways. Its liveliness token is turned
into the status at `home/health/{unit}` by the supervisor. Its health
events go to `home/health/{unit}/event`
([Bus payload conventions](#bus-payload-conventions)). The status
belongs to the supervisor. Degradation is read from status and events;
the unit does not assert it. The recorder keeps every event, queryable
by key and window ([Read path](#read-path)).

- Input a unit cannot use does not crash it. It always leaves one
  `drop` event with a `reason` and enough context to find the source
  (key, topic, device). Reasons include `malformed-payload`,
  `invalid-command`, `unknown-device`, `non-scalar`, and the adapter's
  own. A silent drop is a bug. A crash loop on poison input and data
  vanishing without a word are both worse than a line in the trail.
- Conditions are reported on transition, not per occurrence. A
  persisting condition is reported when it starts and, if it has an
  end, when it ends (`backend-outage`/`backend-restored`, or one
  `device-silent` per transition to down). A stream that repeats every
  tick fills the events table and cannot be folded into intervals. A
  success with nothing to say stays silent.
- Kinds are defined per producer; there is no closed vocabulary. Each
  unit documents its own. The SDK's kinds (`restore-failed`,
  `source-dropped`, `source-restored`) and the recorder's are shared by
  every house.
- Health events are not mirrored. They are a stream of what happened,
  so one published before the recorder subscribes is gone. Anything a
  late joiner must see as current belongs in state.

### Availability

A device that drops out behind a live adapter is a different problem
from unit liveness. The mirror serves a bare value forever. Publishing
on transition makes silence ambiguous, because the bus cannot tell "no
change" from "no sensor". Only the party with protocol knowledge can
tell (a bridge's availability report, a TCP session, a firmware's known
cadence), so availability is decided in the adapter.

- Availability is ordinary state. `available` (bool) is a base aspect
  that applies to every capability. The entity's owner publishes it on
  transition. Recorded history, the mirror, a deviation visible to the
  family on `false`, and automations that subscribe to it all follow
  without extra work.
- Availability is opt-in. An adapter with a real loss signal publishes
  it. One with nothing to say does not fake one. A signal based on a
  receive timer makes the timeout a parameter, since the cadence is
  house knowledge, and reports the down transition as a health event.
- Values go stale rather than being replaced. On device loss the
  values stand and `available` flips. The adapter never publishes invented
  values, nulls or cleared keys. One boolean beside the values is
  better than a tri-state spread across every aspect.
- `available` is reserved. An adapter whose passthrough could create
  it from a native field drops that field (`reserved-aspect`).
- What happens to commands toward an unavailable entity is up to each
  adapter. It may refuse them as `device-unavailable`, or leave them
  for the device to miss.
- No information is a state of its own. An entity with no `available`
  key is unknown, not up. Watching another entity's availability is a
  declared `[bus.subscribes]` binding, seeded from the mirror like any
  other. It is never implicit, because showing that surface is what a
  manifest is for. There is no SDK helper that answers up, down or
  unknown.

A TTL on the mirror would not work, because the core cannot know a
producer's cadence, and a lock can correctly stay silent for months.
The samples' stamps do not settle it either. Age without cadence
answers "when", not "should I trust this", and a value published on
transition is supposed to be old.

`available` has a real limitation. It reports device liveness, not
data freshness. A bridge's passive check on a battery device can take
hours, so a motion sensor that dies while reporting
`occupancy = true` stays trusted until the check runs. Input with a
bounded age still needs the consumer's own policy.

### Staleness

Whether an input is too old to use, and what to do then, is the
consumer's policy, because it is house behaviour. The core enforces no
age anywhere, and no adapter invents one on a consumer's behalf.

- `Freshness` (`homeostat.freshness`) keeps the books per source.
  `seen(source, value, age_s)` records on the monotonic clock.
  `fresh(max_age_s)` returns what is within the automation's window at
  recompute time. `forget()` drops a source, for example on
  `available = false`.
- A catch-up value carries the mirror's age, and a replayed value its
  age since it was recorded, so a six-hour-old reading is not averaged
  in as new after a restart. A live trigger has age zero, so only a
  handler reachable from a catch-up or a replay must handle an empty
  set.
- The helper has no timer. To react to silence, subscribe to
  `home/clock/minute` and call the same `fresh()`.
- The same rule holds for every class. A forecast's consumer checks
  `issued` ([Forecasts](#forecasts)), a fed input expires in the device
  ([Device feeds](#device-feeds)), and a fusion goes stale
  ([Virtual sensors](#virtual-sensors)).

### One-way senders

A sub-GHz PIR, door contact or smoke detector transmits when something
happens and never sends a "clear". The adapter owns the **hold** that
decides when the assertion stops being true. The missing "off" is a
protocol fact, and an adapter may derive values on its own bound
entities. A momentary aspect decayed by the core would be the TTL that
[Availability](#availability) rejects, under another name.

- The adapter publishes transitions only: `true` on the first
  assertion and `false` when the hold expires. A repeat burst inside
  the hold extends the deadline silently, since these senders repeat
  every burst by design.
- At startup the adapter publishes `false` for every bound entity. The
  held value is the adapter's own construct, and a crash-looping
  adapter must not leave a sensor stuck on. Only an adapter whose
  breaker is open leaves `true` standing, and its health shows that.
  This is the opposite of a latch
  ([Restoring a unit's own last value](#restoring-a-units-own-last-value)).
- The hold is a parameter per aspect rather than per entity, chosen by
  capability and features, with a generic fallback. A contact, a PIR
  and a detector want different holds, and every contact wants the
  same one.
- Availability comes from the bridge, not from a receive timer, because
  silence is a one-way sender's normal state.
- A second one-way adapter would move the timer into the SDK, where
  `Freshness` and `Cooldown` live. It would not move into the core.

Debouncing on the consumer side was rejected. Every consumer would
reimplement it differently, and the recorder could not reconstruct what
was true when.

### Logs and the audit trail

Logs are a by-product. Events are the audit trail.

- Unit output is captured. It is tagged on the supervisor's streams
  and kept in a 500-line ring per unit at `home/meta/{unit}/log`, which
  MCP's `read_logs` and the dashboard's unit detail read. It is lost on
  supervisor restart and is never recorded
  ([The unit contract](#the-unit-contract)). Peripheral logs go to
  their adapter's stdout, tagged with the device, at warning and above,
  so a chatty device cannot drown the ring.
- The durable trail is the recorder's `events` table. It holds every
  health event, every accepted parameter write, and every command
  envelope with its actor and band. It is read through
  `home/history/events`, MCP's `read_events`
  ([Agent surface (MCP)](#agent-surface-mcp)) and the dashboard.

Homeostat has no log sink. Tagged stdout is the standard export
surface. Durability, retention and indexing are deployment
configuration (Docker logging drivers, journald, Loki), and anything
built here would reimplement them worse. If a line matters enough to
query next week, the unit should emit a health event for it. A
queryable log store, or stdout on the bus, would remove the pressure to
do that.

## Surfaces

A surface is where a person or an agent meets the house. Every surface
is a unit on the bus like any other. It is supervised, has health, and
holds only the authority its manifest declares. No surface has a path
around [plan and apply](#plan-and-apply). The dashboard commands at the
manual band and edits family parameters. The agent surface only reads.
Every structural change goes through the house repo.

### Dashboard

The dashboard is an adapter for humans. It speaks HTTP and a WebSocket
to browsers on one side, and the bus through the SDK on the other.
Browsers never speak Zenoh. `adapters/dashboard.py` is a `service` unit
(aiohttp). It serves one hand-editable page, `dashboard.html`, along
with its stylesheet `assets/dashboard.css`, its decision logic
`assets/dashboard-logic.js`, its ES modules under `assets/dashboard/`
(entry `main.js`) and a few vendored libraries. It serves from an
allowlist of filenames plus that one directory, and there is no build
step. The page is rendered in the client because pushing live state is
the dashboard's main job.

- The browser goes through the dashboard unit rather than onto the raw
  bus. Running Zenoh's remote-api plugin in the browser would bypass
  the grant table, the arbiter and the manifest-declared surface, and
  would couple every client to the bus protocol. Going through a unit
  reuses existing plumbing. Commands are manual-band envelopes.
  Parameter edits take the [live parameter](#live-parameters) path. An
  opening page is a late joiner on
  [the last-value mirror](#the-last-value-mirror). Charts query the
  recorder.
- The page is generated from the house's text. It is a pure function
  of the manifests, entity files, `zones.toml`, `dashboard.toml`, the
  grant table and the bus. Layout state exists nowhere else, and there
  is no customisation in the browser, because the project exists to
  avoid hidden UI state. Labels come from `[naming]`; currently the
  `en` names are used.
- The dashboard owns rendering, and adapters never do. Adapters speak
  homeostat vocabulary (capability, features,
  [aspect descriptors](#aspect-descriptors), constraints), and only the
  dashboard maps it to controls. Per-adapter UI was rejected because
  widgets would drift and the page would stop being a function of the
  house's text. A device class that needs a new control extends the
  public vocabulary and adds one rendering, which every adapter then
  gets.

The unit declares `watches = "house"`
([Change detection](#change-detection)). It also re-parses the house on
`/api/model` at most every two seconds. If a half-written file fails to
parse, it keeps the last good model.

The whole HTTP API sits behind the gates in
[Local-only access](#local-only-access).

- `GET /api/model` is the house rendered for the browser. Each entity
  is marked `commandable`. Each unit lists what it drives and reads.
  The model also carries the views and the `driven` bands.
- `GET /ws` sends a snapshot of state, forecasts, holds, health, config
  and descriptors, followed by live deltas.
- The writes are `POST /api/cmd`, `/api/lights/off` and `/api/param`.
- The rest are read proxies onto the recorder, the log tail and the
  camera relay, plus the map extract.

Live state reaches browsers through one bounded outbox per client (256
messages). A browser that stops reading is closed rather than buffered
for. On reconnect it takes a fresh snapshot, which covers what it
missed. The unit subscribes before it seeds from the mirror, so a live
update always supersedes the seed.

#### Commanding

Every command leaves at `priority = "manual"`, so the family wins at
the [arbiter](#arbitrated-mode), and manual-band writers never count
toward exclusivity ([Write modes](#write-modes)).
The manifest declares a blanket `home/cmd/**` publish for each
capability the dashboard may command.

- The dashboard honours its own grant table. Nothing on the bus
  re-checks grants, so a blanket publish granted for `light` could
  carry a `climate` setpoint. The dashboard derives the capabilities it
  may command from its own cmd-class publishes and refuses `/api/cmd`
  for any other. It marks each entity `commandable`, so controls
  without a grant render inert. This is the unit keeping to its
  declaration. It is not a security boundary
  ([Local-only access](#local-only-access)).
- The dashboard may command a capability's base aspect or a declared
  feature. These are type-checked, so a JSON object never travels in an
  envelope. It may also command an aspect whose descriptor declares a
  family-editable command, checked against the descriptor's constraint.
  Otherwise bounds are left to the adapter.
- Group actions fan out at the manual edge. `POST /api/lights/off`
  sends one manual-band off per commandable light, whether it is lit or
  not. A commandable "scene" entity was rejected. Its owner would
  republish at the automation band ([Priority bands](#priority-bands)),
  and it would be a second automation-band writer on every exclusive
  light.

#### The stages of a command

A command is a proposal, not a write ([Cmd envelopes](#cmd-envelopes)).
So the page never paints the request as the device's state. For
example, an out-of-range setpoint returns `ok` and is then dropped by
the adapter. The control shows the request as a request, with a line
naming the stage ("asked 22.5° · still 21.0°") and then the outcome.

- Taps build on the request, not on a readback that has not moved yet.
  They settle for 600 ms before one command goes out. Each request
  replaces the previous one for that aspect.
- The page knows at once if nobody heard the command. `/api/cmd`
  replies with the envelope's `id` and `heard`. `heard` means the
  owning unit holds its liveliness token and something subscribes
  where it listens: the arbiter's forward key for an arbitrated entity,
  otherwise the command key. Liveliness is checked first, because the
  recorder subscribes to every command.
- Outcomes:
  - The asked value coming back **confirms** the command.
  - A value held before the first tap, or asked for on the way, is
    progress. A polling bridge republishes the old value until the
    device moves.
  - Any other value is **adjusted** (clamped or rounded), and both
    numbers are shown.
  - An arbiter `refuse` carrying the id is **held**. It is worded as a
    lost contest, since a retry would lose the same way.
  - An adapter drop carrying the id shows its reason.
  - Nothing within the wait is **no confirmation from the device**.
- Matching is by `id`. Matching by key and value would cross wires
  when someone taps twice. Values compare at the control's grain, so a
  bulb that rounds by one step of 0–254 is not shown as adjusted.
- The wait is the descriptor's `readback_s` (per command, then per
  entity). Without one it is a generous guess per capability. A timeout
  that fires early reports a failure that did not happen.

Two things are not built. A "delivered to the device" event would
require every adapter to emit it. Correlating readbacks to commands
would leave each adapter to decide which readback answers which
command.

#### Charts, forecasts and sources

Charts query the recorder through `/api/history`
([Read path](#read-path)). For a number they ask for a `bucket` sized
to one point per drawn column. For a boolean or enum they ask for
`changes`. The descriptor decides which, so an enum coded as integers
is not averaged. `class=cmd` draws a "Commanded" strip under the chart.
It is the page's one view of intent against outcome.

Forecasts reach the page live with state rather than from the
recorder. The family sees what the house believes now, and a house
with no recorder still sees its horizon. The current belief is drawn
dashed past the now line and captioned with when it was issued.
Staleness is for the consumer to judge ([Forecasts](#forecasts)), and
the dashboard's policy scales itself. A belief older than the span it
still has to cover is drawn grey and marked `stale`. A belief whose
horizon has run out is no longer drawn as the future.

In the detail overlay, the `forecasts` view draws the stored issues
(`/api/forecasts`, the newest 40). The `sources` view draws each
declared contributor ([Sources](#sources)) with its exclusions from
`/api/source-events`. Each issue or contributor is its own neutral
line, and the outcome keeps the accent colour. Neither view draws an
envelope, because its edge would trace a path nobody predicted. Both
views are owner work, so they live in the overlay rather than as
widgets.

#### Controls and the overlay

The page maps descriptor vocabulary to controls and adds no
vocabulary of its own. A command the family may not edit reads as a
value.

- A derived grain is a twentieth of the range, rounded to something a
  person would say. Where the house knows better, `dashboard.toml`
  declares a `[[control]]` step. The step is keyed by what is
  controlled rather than by the widget placing it, so one grain holds
  everywhere. It is not a layout hint, since a step says what a control
  does and not where it sits.
- To guard against accidental input, a family parameter's caption
  prints its manifest default (`0–60 · default 5`). Range inputs take
  `touch-action: pan-y`, so a scroll that starts on a thumb does not
  command a device.
- Every parameter is visible. An owner-level parameter that is off its
  manifest default counts as a deviation, so a house running off its
  manifest can be told apart from one running it. Only
  `editable_by = "family"` parameters get a control.

Tapping an entity, a reading or a unit opens a detail overlay. Its
width is a per-viewer `localStorage` preference. Two rules apply to
anyone changing the overlay:

- A reader's choices live outside the markup. Live state re-renders
  the panel, so a pinned issue or highlighted source held only in the
  DOM would be silently lost.
- Nothing round goes inside a chart SVG. The SVG stretches with
  `preserveAspectRatio="none"`, so value dots are positioned in the
  wrapper by percentage.

#### The page

`dashboard.html` and its own assets are one artifact, served with
`Cache-Control: no-cache`. Heuristic freshness could pair a new page
with old cached logic after an upgrade, giving a page that renders
empty over a healthy backend. Versioned asset URLs were not used,
because the version would have to be rewritten into a hand-edited
file.

Pure decisions (arithmetic and selection, never markup) live in
`dashboard-logic.js`, and `node --test tests/js` pins them. Markup is
written with the `html` tag (`assets/dashboard/html.js`). The tag
escapes every interpolated value unless the value is itself markup, so
a forgotten escape cannot turn a label from the house into markup.
`tests/browser` drives the real page against canned fixtures. A canary
in `tests/dashboard.rs` checks the fixtures' field names against a real
`/api/model`. The browser suite is a broad net and does not replace
opening a browser on a change.

### Views are text

`dashboard.toml` at the house root lists the views. Each `[[view]]` is
a nav entry. It holds either an ordered list of widgets from a closed
vocabulary or a generated view (`kind = "now" | "setpoints" |
"rooms"`), but not both. [docs/widgets.md](widgets.md) shows each
widget and [docs/manifest.md](manifest.md) has the fields. The core
validates the file at `plan`, as it does `zones.toml`, and never
renders it. It is a house-wide input, so a view edit is a visible
change that restarts the dashboard.

- The file replaces the nav rather than adding to it, so a house can
  say "these three views are the dashboard". Without the file the
  dashboard renders `Now`, `Setpoints` (every family parameter) and
  `Rooms`.
- Nothing becomes unreachable. Two pages are fixed and never appear in
  the file. **Health** shows unit status and breakers, and is visible
  to the family by design. **Not shown** lists every entity that no
  widget places, drawn as usable room cards.
- A view shows its text. A read-only **Text** button renders the
  `[[view]]` block behind a view. That gives a name to use when asking
  an agent working in the house repo. The dashboard never writes to the
  house.
- Only `dashboard.toml` places things. An entity file carries no
  `[dashboard]` table, so there is one way to place a thing.
- `group` composes, one level deep. Nested groups and per-widget layout
  hints (`span`, column counts) are refused. Either would turn the file
  into a layout language, and layout belongs to the dashboard.

`Now` shows the error signal rather than an inventory: people, the
deviations feed and the map. A house in equilibrium renders a nearly
empty page. The `deviations` feed draws from:

- supervision: any unit not running;
- notable state from the capability vocabulary (lights on, as one row
  with the "All off" action), any entity with `available = false`
  ([Availability](#availability)), and any described aspect marked
  `notable` that reads true. These come from vocabulary, never from
  house configuration;
- parameters whose live value differs from the manifest default;
- arbiter holds that displaced somebody.

The arbiter leases every forwarded command, so most holds are just the
house working. A hold is listed as a deviation when it displaced
somebody: when it stands at a band above the lowest band at which
anything is granted to command that aspect (`driven` in `/api/model`).
Two other rules were rejected. "Once it has refused something" depends
on how often the displaced automation publishes, which is a fact about
its author. "Every hold" would list a family locking a door that no
automation touches, which displaced nobody. Possession still shows on
the control as **held**.

The unit card is a pure function of the manifest and the grant table.
It shows family setpoints, published entities, what the unit drives
(its cmd grants) and what it reads (its expanded state subscriptions).
The last two are `{entity, aspect}` rows, because a relation is per
aspect. If the card is wrong, the manifest is wrong.

### Map and people

A person is an entity with `capability = "person"` in the pseudo-room
`person`. People move and the key space is keyed by room, so the room a
person is in is state, not structure. Location is a set of scalar
aspects ([the capability vocabulary](#the-capability-vocabulary)), so
position history comes for free. The `people` widget reads `presence`
and falls back to the age of the last fix.

The `map` widget is a view over every entity with a location. Tiles
come from a self-hosted PMTiles extract named by
`HOMEOSTAT_DASHBOARD_TILES` and served by the dashboard unit. A public
tile CDN would learn family positions from tile coordinates.

### Cameras

Pixels are the media plane, and detections are data. The payload
conventions, the recorder and the mirror all assume small scalar JSON.
If video bytes entered a homeostat process as data, the core would no
longer be small.

- The event plane is on the bus. A camera is an entity
  (`capability = "camera"`) that publishes scalar aspects, currently
  `motion`. It is recorded and automatable in the same way as a PIR's
  `occupancy`. A better detector later changes one adapter and no
  automation.
- The media plane is off the bus and never recorded. Live viewing goes
  over RTSP into go2rtc, with one upstream session per camera however
  many viewers there are, restreamed as a pure remux. Stream names
  equal entity ids.
- Browsers never speak to go2rtc. Its API is unauthenticated and can
  add streams, read back RTSP URLs with credentials and run `exec:`
  sources. So go2rtc binds to `127.0.0.1` with its other listeners off.
  The dashboard relays `/api/camera/{entity}/live` byte for byte to
  go2rtc's `api/ws` behind the dashboard's own gates, and forwards only
  the player's MSE request. Relaying is not processing. The bus, the
  recorder and the core stay scalar.
- The player uses MSE. WebRTC's direct peer connection cannot go
  through the relay, and MSE's 0.5–1.5 s latency is fine for a glance.
  There are no snapshots, because a still from H.264 needs a ~100 MB
  transcoder that the image does not carry. The stream starts only on
  tap.

A Go binary cannot declare a liveliness token, so a shim owns the token
for it. `adapters/go2rtc.py` renders the go2rtc config from
`HOMEOSTAT_CAMERAS` into a `0600` file outside the repo, spawns the
binary, and polls its API until it answers. Only then does it declare
ready. If the child dies, the shim exits and the supervisor backs off.
This shim pattern is the general answer for any foreign binary.

An NVR, motion detection, transcoding and frame storage are kept out
of homeostat. Mature tools do each of them better. A detector such as
Frigate is the growth path, as one more adapter.

### Notifications

Reaching a person means reaching a device the house binds. `notifier`
is a capability. An entity file per addressee binds it to a delivery
adapter. An automation that wants to reach someone declares an
ordinary cmd-class publish onto that entity. The core adds nothing
beyond the vocabulary row.

- `message` (base) and `alert` (feature) are commandable strings that
  carry the text itself. Severity is an aspect rather than a payload
  field. That way the two are granted separately, policed separately
  by the adapter (quiet hours may withhold `message` but never
  `alert`), and recorded separately. `delivered` is the epoch time at
  which the delivery service acknowledged the last message. It does not
  mean a person has read it. Chat ids, topics and priorities are the
  adapter's dialect.
- The entity is the address. One person's phone is in the pseudo-room
  `person`, and a group channel is in `global`. A person with two
  channels is two entities, and switching providers changes no
  automation.
- The grant table gates notifications without any change. A new
  `notifier` publish is a grant delta, so the plan is
  [structural](#tiers) and shows who may reach whom. Channels are
  `shared`, so the band has no effect. `actor` is what the recorder
  keeps with every message.
- Rate limiting has two parts. The cooldown is house policy: a
  family-editable parameter kept by the SDK's `Cooldown`. The adapter's
  own floor is a second line of defence, and drops with
  `rate-limited`.
- Failures are reported. A delivery adapter verifies its server before
  `ready()`. An undelivered message is a `drop` with `delivery-failed`.
  The channel's `available` goes false until the next success, which
  shows as a deviation on `Now`.

Rejected alternatives:

- An external subscriber cannot carry intent. "Skipped because it
  rained" cannot be derived from state.
- Health events have no addressee.
- A `home/notify/**` class with a routing service would make the
  addressee a name the plan cannot check.
- An SDK facility would grant authority by import.
- Dashboard web push has no secure context, and looking at the
  dashboard is not the same as being told.

### Agent surface (MCP)

`homeostat mcp` is an MCP server through which an agent observes the
house. It is read-only and only a bus client. It takes no house root,
never reads the repo, and never shells out to git. An agent changes the
house the way everyone does: it edits the house repo and runs
`homeostat plan`, and the owner applies.

| Tool | Reads |
|---|---|
| `read_state` | any `home/**` key expression through the core's last-value caches |
| `read_history` | `home/history/{state\|cmd}/{entity}/{aspect}` with `from`/`to`, `limit`, and the folds `bucket` or `changes` ([Read path](#read-path)) |
| `read_logs` | a unit's captured output ring buffer |
| `read_events` | the audit trail at `home/history/events` |
| `schema` | the manifest contract as JSON Schema |
| `explain` | the registered paragraph for a validation error code |

`schema` and `explain` let an agent that writes manifests read the
rules the validator enforces instead of its source
([The manifest is the contract](#the-manifest-is-the-contract)). The
discovery loop is described in [Discovery](#discovery).

- Transports. Stdio (`--bus <endpoint>`) is launched by an MCP client
  for local work. HTTP (`--http <addr>`) serves a deployed house as a
  `service` unit the house opts into, so it is supervised like any
  unit. The HTTP transport is stateless streamable-HTTP: a POST carries
  one JSON-RPC message and gets `application/json` back, and GET
  returns 405. It sits behind the dashboard's gates
  ([Local-only access](#local-only-access)).
- The protocol is hand-rolled: `initialize`, `tools/list`, `tools/call`
  and `ping`. An MCP SDK would be the largest dependency in the tree
  for four methods.

There are no write tools. Every agent in use works in a checkout of the
house repo, where the CLI already gives it plan and apply with git
review. A write path that commits into the supervised tree would put
unapproved code where units spawn from. A pending plan gates the
restart but not the file, so the new code would run at the next crash.
A write side would first need proposals staged outside that tree, and
the tier ceiling enforced in the supervisor's apply path.

### Voice

Voice is planned but not built. The plan is:

- a narrow, high-precision fast-path intent matcher, with the
  conversational agent as fallback;
- a fast-path grammar generated on the house side from manifests and
  the key space at plan/apply time, so the public tool never sees
  private naming;
- local wake word and speech-to-text, with no cloud in the fast path;
- short-lived agent sessions scoped to one satellite.

A satellite is a manual-band, family-tier surface like the dashboard,
and fans group commands out at the edge.

## Security model

Homeostat has no accounts, no login and no TLS. Strangers are kept out
because the house's surfaces are reachable only from its own network.
A family member cannot rewire the house because no surface they can
reach has a structural path. Every new surface keeps both rules.

### Local-only access

Being able to reach a surface is the credential. The house is reached
on its LAN, or over WireGuard for phones and remote devices. Anything
that can reach a surface is treated as the family
([Family tier only](#family-tier-only)), so the gates below are
structural and are not authentication.

The bus port matters most. A cmd envelope's `priority` and `actor` are
self-declared and checked only for shape
([Bus payload conventions](#bus-payload-conventions)), and nothing on
the bus re-checks grants. Anything that can publish on the Zenoh port
(7447) can command every entity, outbid the arbiter by claiming the
top band, and forge state. So the bus is never published to the
network:

- The starter's compose file publishes the dashboard and the MCP port
  but not 7447. `plan` and `apply` run through `docker compose exec`.
  Binding to `127.0.0.1:7447` is not a boundary either, because a
  container on `network_mode: host` shares the host's loopback.
- Grants describe what a unit declared, not what it can do. A unit that
  opens its own session can publish anything. Making grants constrain
  units takes a bus credential per unit; a check in each adapter would
  not do it.

The browser is not local, even when the dashboard is. A public page
open in a family member's browser can fire requests at LAN addresses
(CSRF). Through DNS rebinding it can also make the browser treat a
house address as the page's own origin and read the replies. So every
HTTP surface has three gates:

| Gate | Dashboard | MCP over HTTP |
|---|---|---|
| `Host` is a house-network address or a listed name | every request | every request |
| `Origin`, when present, passes the same host rule | WebSocket handshakes | every request |
| `X-Homeostat` header present | every `POST` | every request |

- The `Host` check defeats DNS rebinding, because a rebound public
  domain arrives under its own name and is refused. A house-network
  address is a private, loopback, link-local or unspecified one (IPv6
  unique-local included), which is where a LAN or a WireGuard tunnel
  lands. The listed names are `localhost`, `homeostat`,
  `homeostat.lan` and `homeostat.local`. They are extended through
  `HOMEOSTAT_DASHBOARD_HOSTS` and `HOMEOSTAT_MCP_HOSTS`, not through
  the repo. Both implementations are tested against one table in
  `tests/fixtures/host_gate.json`.
- `Origin` is sent on a WebSocket handshake and on a cross-origin
  request. For a foreign page or `null` it fails the host rule.
- `X-Homeostat` is a header that a cross-origin `fetch` cannot add
  without a CORS preflight, and nothing answers the preflight. Without
  the header, a cross-origin `text/plain` POST is a "simple request"
  that reaches the server without a preflight and could drive a write
  blind. The MCP server requires the header on reads too, because what
  it serves is the house's private record.

The MCP server refuses before reading a body, never echoes the reason
to a browser, and bounds what a LAN peer can make it hold. The
dashboard caps bodies at 64 KiB. A foreign service with an
unauthenticated API binds to `127.0.0.1` and is reached only through a
unit's relay, as go2rtc is ([Cameras](#cameras)).

The dashboard is plain `http` and a bookmark, with no PWA. Service
workers need a secure context even on private addresses. A private CA
is a plausible later path, and nothing in the architecture depends on
it.

Secrets never enter the repo, because a repo is copied, pushed,
reviewed and read by agents ([Repo split](#repo-split)). A unit sees
only a fixed base environment plus the variables its manifest names in
`[runtime] env` ([The unit contract](#the-unit-contract)), so a token
meant for one unit never reaches another. Per-device secrets live in a
TOML file outside the checkout, named by an environment variable
(`HOMEOSTAT_CAMERAS`, `HOMEOSTAT_MQTT_CREDENTIALS`). Files a unit
renders from them are written `0600` outside the repo and deleted on
exit. A non-secret endpoint is ordinary repo content in `[discovery]`.

### Family tier only

Anyone who can reach the dashboard is `family`. The dashboard has no
owner mode, no admin panel and no approval surface, and it will not get
one. The owner acts through git and the CLI (`plan`, review, `apply`).
The dashboard can do what the family tier may do and nothing more:

- send manual-band commands within its own grants, checked against the
  vocabulary or the adapter's declared constraint
  ([Commanding](#commanding));
- write `editable_by = "family"` parameters within their constraints
  ([Live parameters](#live-parameters));
- read state, history, health and logs.

Nothing structural (a grant, a manifest, an entity binding, a unit's
code) is reachable from the dashboard. A stolen phone inside the
perimeter can nudge setpoints and switch lights, but cannot rewire the
house. The agent surface holds the same line by being read-only
([Agent surface (MCP)](#agent-surface-mcp)). Voice will be family-tier
on the same terms.

## Distribution

### Repo split

- The public repo (`homeostat`, this repo) holds the Rust core, the
  manifest schema ([docs/manifest.md](manifest.md), versioned by each
  file's `schema` field), the Python SDK, the generic units in
  `adapters/`, and `examples/starter-house`, the template a new house
  starts from. The plan test corpus, a larger house of manifests
  without code, is `tests/fixtures/house_reference`.
- The private house repo holds every manifest, entity file and zone,
  the automations, `dashboard.toml`, pending plans, and house-specific
  agent instructions. It pins a release (image tag and SDK version).
  Its CI can run `homeostat plan`, which without a bus plans offline
  and exits non-zero on any validation error.
- To decide which side something belongs on: a device address, a
  family member's name, a room name or a behavioral choice is private.
  Anything that would be identical in a stranger's house is public.
  Generic automations graduate into SDK helpers or adapters. The public
  tool sees a private repo only locally.

### Release artifacts

A release (tag `vX.Y.Z`) publishes the following, each with a signed
build provenance attestation:

- `homeostat` tarballs for `x86_64` and `aarch64` Linux;
- the SDK wheel;
- the image `ghcr.io/freol35241/homeostat`, tagged `X.Y.Z` and `X.Y`,
  for `linux/amd64` and `linux/arm64`;
- `SHA256SUMS` for the tarballs and wheel.

The binary reports its version and commit at `home/meta/system/about`.
The version lives in `Cargo.toml`, `sdk/python/pyproject.toml`, the
starter's compose file and `scripts/sync_starter.sh`. The script's
check fails when they disagree, because reporting a wrong version is
worse than reporting none.

### The container image

The image (`Dockerfile`) is the deployment boundary. One container
holds the core and every unit as plain processes
([Process model](#process-model)). It carries:

- the `homeostat` binary;
- `git`, for `plan --save` and apply's commit provenance;
- `tini` as PID 1;
- `tzdata`;
- uv with a pre-installed CPython 3.12, so first boot downloads no
  interpreter;
- the SDK wheel in `/opt/homeostat-wheels`, with `UV_FIND_LINKS`
  pointing there;
- the `go2rtc` binary that the camera unit spawns, checksum-pinned per
  architecture.

Provisioning a binary is the image's job and never the house repo's.

The container runs as an unprivileged user (uid 1000, or any uid via
`--user`). The house repo is at `/house`. `/var/cache/uv` is worth a
volume, so unit environments survive container replacement. The
default command is `up /house --listen tcp/0.0.0.0:7447`, and
`HOMEOSTAT_BUS` is preset to loopback, so
`docker exec <container> homeostat apply /house` needs no address.
Without Docker, a house runs from the release binary and the wheel,
with `UV_FIND_LINKS` pointing at the wheel's directory.

Port 7447 is exposed for sibling containers but must not be published
to the host, because reaching the bus is full authority over the house
([Local-only access](#local-only-access)). Host networking is not
required, since the bus uses explicit endpoints. An adapter that
discovers by multicast (mDNS) sees only what reaches the container's
network, and falls back to explicit addresses.

### SDK distribution

A house unit names the SDK by exact version in its PEP 723 block,
`"homeostat==X.Y.Z"`, with no `[tool.uv.sources]`. uv resolves it from
the bundled wheel through `UV_FIND_LINKS`.

- The pin is in the unit script, so `files_hash` covers it. An SDK
  bump is a visible behavioral change in `plan`, and `apply` restarts
  the unit. A vendored SDK copy and a floating or path dependency were
  rejected, because both sit outside change detection.
- First boot needs no clone and no network. A git source would need
  both, and "pinned to a tag" invites the same-commit trap described
  below.
- The cost is that a house pinning a version the image does not bundle
  fails to resolve at unit start. This is the version-floor hazard in
  its loudest form: the unit never reaches `running`, and its log says
  why.

Inside this repo, `adapters/` and test fixtures use an editable `path`
source, so tests exercise the working-tree SDK. Each adapter script has
a uv lockfile beside it (`{script}.py.lock`), so a unit resolves the
dependency versions its release was tested against.

An adapter and the SDK must come from the same commit. An adapter from
`main` fails against an older SDK with `AttributeError`, so a house
copies an adapter from the release it pins.

### The starter house

`examples/starter-house` is a self-contained house repo. Copy it out,
run `git init`, and run its compose file (mosquitto, Zigbee2MQTT and
the image). Its copies of the generic adapters, their lockfiles and the
dashboard's assets are generated and never edited.
`scripts/sync_starter.sh` takes each one from `adapters/` at the
release tag the starter pins (`SDK_TAG`). It rewrites the SDK source to
the `homeostat==X.Y.Z` pin, and the lockfile's SDK entry to the bundled
wheel. The starter is therefore a snapshot of a release, not of `main`.
CI's `sync_starter.sh --check` fails when any copy differs from what
that release generates. `scripts/release.sh X.Y.Z` bumps `SDK_TAG`
along with the other version strings, relocks, and regenerates the
starter. Until the tag exists, the check compares against the working
tree, which is the release commit itself. CI checks out full history,
because otherwise the tag is not found.

## Open questions

These are questions the design has named but not answered, and known
gaps it has not closed.

- **Should `features` gate command contents?** The grant table does not
  check a command's value or aspect against them; the SDK and the
  adapter do. The current leaning is no separate layer. See
  [The grant table](#the-grant-table).
- **Access control on the bus.** Reaching the bus is full authority.
  Zenoh ACLs would make grants a runtime boundary. When to take that on
  is open. See [Local-only access](#local-only-access).
- **`editable_by` is enforced by the dashboard only.** The core's
  config queryable checks type and constraint, so any bus client may
  write an owner-tier parameter. See [Live parameters](#live-parameters).
- **Turning a live edit into a commit.** A family member's live edit
  is reverted by the next apply unless someone commits it. Capturing it
  automatically is not built. See [Live parameters](#live-parameters).
- **Handing an arbiter hold back early.** Only expiry ends a hold.
  Whether release belongs on the envelope or on an arbiter surface is
  open. See [Arbitrated mode](#arbitrated-mode).
- **Forecast uncertainty and corrections to the past** have no
  representation. A point is one scalar, and `state` keeps no
  reanalysis. See [Forecasts](#forecasts).
- **An inhibit class for interlocks**, a lockout held while a condition
  holds and honoured by the arbiter against every band, is deferred
  until a second case needs it. See
  [Burners and interlocks](#burners-and-interlocks).
