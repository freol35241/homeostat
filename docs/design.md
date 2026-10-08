# Homeostat design

How homeostat works, and why it is built that way. The document
describes the system as it is: a change that makes a sentence here
untrue fixes the sentence in the same commit. How each decision was
reached is in the git history, not here.

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
W. Ross Ashby's 1948 machine, and the name is the design argument: the
system is a regulator holding the house in equilibrium, not an
assistant waiting for commands. The family adjusts its setpoints; the
owner governs the regulating machinery. In the code that split is
literal: parameters are family-editable at runtime, structure changes
only through the repo and [plan and apply](#plan-and-apply).

It replaces Home Assistant for one owner's actual devices (Zigbee
through Zigbee2MQTT, ESPHome, MQTT), not the long tail. What it is for:

- **Text configuration first.** The house repo is the system of record.
  No UI mutates hidden state; the running system is derived from the
  repo plus the current state of the world.
- **Automations are plain code.** A Python script against a small SDK,
  with no DSL to outgrow.
- **Agents maintain it the way people do.** An agent reads the same
  files, runs the same `plan`, and its changes reach the house through
  the same commit-and-apply path as a human's.
- **A small core.** The core owns the key space, validation, the grant
  table, plan and apply, and supervision. Everything that speaks a
  device protocol or keeps data lives in a unit outside it.

There is no Home Assistant bridge.

## Architecture

Four parts: a Rust core, a Zenoh bus, the units it supervises, and the
house repo they are all derived from.

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

The `homeostat` binary. Offline (`homeostat plan`) it validates a
house repo, expands templates and zones, resolves the grant table and
prints a plan. Running (`homeostat up`) it is the bus router, the
process supervisor, the owner of the live parameter store and the
last-value mirrors, and the executor of apply walks; `homeostat mcp`
serves the read-only agent surface. It parses no device protocol and
keeps no history.

### Bus

Zenoh. Pub/sub carries state and commands; queryables answer reads
(last values, history, parameters) and the two writes that need a
synchronous answer, a parameter write and an apply. The supervisor's
session is a router on a fixed endpoint; every unit and observer
connects to it as a client, scouting off, so topology is explicit and
parallel test buses never find each other. Keys and payloads are in
[Key space](#key-space).

MQTT device traffic stays on its own broker (mosquitto), reached only
by the adapters that speak it. Not the Zenoh MQTT plugin or bridge:
broker retain is load-bearing (an OwnTracks phone's last position,
Zigbee2MQTT's device inventory) and the plugin does not document it;
mapping device topics into `home/**` would end adapters being the only
membrane between a dialect and the bus; and plugin mode would parse
foreign protocols inside the supervisor.

### Units

Every running thing besides the core is a unit: an `adapter`,
`automation` or `service` ([Unit kinds](#unit-kinds)), with one
manifest schema declaring everything it may touch ([Units and
manifests](#units-and-manifests)). Python units are `uv run` scripts
whose dependencies are a PEP 723 block in the script, so each has its
own hermetic environment; Rust units are binaries. The SDK
(`sdk/python`, package `homeostat`) gives a Python unit its session,
typed key builders, the automation `Context` and the command envelope.

### Process model

Units are plain OS processes supervised by the core
([Supervision](#supervision)), in the Erlang lineage: a process
boundary buys fault isolation and a language boundary, not a
microservice. They are not containerized individually; the whole
system may run in one container ([Distribution](#distribution)).

### The house repo is the system of record

Nothing running edits the repo's manifests, entity files,
`zones.toml` or `dashboard.toml`; the only things written into the
checkout are a unit's own data (the recorder's store under the
gitignored `data/`) and pending plans under `plans/`. The running world
is diffed against the repo, never the reverse: a change is a commit,
then [plan and apply](#plan-and-apply); a live parameter edit is drift
the next apply resets ([Live parameters](#live-parameters)); and
`home/meta/system/applied_commit` names the commit that is running.

### Unit granularity: the atom is the unit

The unit is the atom of **authority** (its grants, subscriptions,
publishes and parameters), of **failure** (its liveliness token,
backoff, breaker and process group) and of **change** (its
`files_hash` and its step in the apply walk). Those three boundaries
coinciding on one process is what the process model buys.

What a unit contains is the author's call: a script may host several
rules, and its manifest declares the union of what they need.

- **Group by shared blast radius, not size.** Rules share a unit when
  they should live and die together: they share a restart and a health
  key, and an edit to one is a behavioral change to all. "Evening
  lighting" and "the heat-pump setback" are two units, however small.
- **Authority is the union.** The grant table sees units, not rules,
  so a rule that must not touch what its neighbour touches is a
  separate unit.
- **Cost.** A minimal Python unit is about 12 MB resident; forty are
  about 0.5 GB, fine on a NUC or a Pi 4. If that binds, bundle by the
  rule above.

Not a multi-tenant automation runner (one service hosting many rules
with a scheduler): it shares authority and failure across rules that
did not choose to, and with per-rule health and restart it is the
supervisor rebuilt in Python.

## Glossary

The words this document, the code and the error messages use, each in
one sense; where a word carries two, both are listed.

- **House.** One home's configuration, and the system running it.
- **House repo.** The git repository holding a house as text: unit
  manifests, entity files, `zones.toml`, `dashboard.toml`. The system
  of record the running world is diffed against.
- **Core.** The `homeostat` binary: validation, plan and apply, the
  supervisor, the last-value caches, the MCP server.
- **Unit.** Every running thing other than the core: one process, one
  manifest in `units/`. An **adapter** puts devices on the bus, an
  **automation** regulates, a **service** is infrastructure with no
  entities. See [Unit kinds](#unit-kinds).
- **Entity.** One thing in the house with state, declared by an entity
  file. Its name is the file stem and is unique across the house.
- **Binding.** (1) The relation between an entity and the one unit that
  embodies it: an adapter for a device, an automation for a virtual
  entity. That unit is the entity's **owner**. (2) A **binding name**:
  the key of an entry in a manifest's `[bus.subscribes]` or
  `[bus.publishes]`, which the SDK takes instead of a key expression.
- **Capability.** What kind of thing an entity is (`light`, `lock`,
  `sensor`, …), from a fixed vocabulary. It decides the base aspect,
  which commands are grantable, and the dashboard widget.
- **Aspect.** One named value of an entity: `on`, `brightness`,
  `temperature`. The last segment of a state key.
- **Base aspect.** The aspect a capability's commands and widget act
  on (`on` for a light). A capability without one takes no commands.
- **Feature.** An optional aspect beyond the base that an entity file
  declares (`brightness` on a light).
- **Aspect descriptor.** An adapter's description of an entity's
  aspects (label, kind, unit, group, commands), carried in its
  discovery record. See [Aspect descriptors](#aspect-descriptors).
- **Room.** Where an entity is; a key segment. **Pseudo-rooms**
  `global` and `person` hold entities with no place.
- **Zone.** A named set of rooms, written in a key's room slot and
  expanded at plan time. Zones never appear in a published key.
- **Key class.** The second segment of every bus key: `state`, `cmd`,
  `arbiter`, `forecast`, `config`, `meta`, `health`, `clock`,
  `history`, `discovery`, `hold`. See [Key space](#key-space).
- **Grant.** A unit's publish resolved against the entities it reaches.
  The **grant table** is all of them: the permission record `plan`
  shows, and the dependency graph that orders an apply.
- **Band.** A command's priority: `automation` < `agent` < `family` <
  `manual`, declared per publish in the manifest, never per command.
- **Write mode.** How commands to an entity are governed: `shared`,
  `exclusive` or `arbitrated`. See
  [Capabilities, grants and write policy](#capabilities-grants-and-write-policy).
- **Envelope.** A command's payload: `{value, priority, actor, id}`.
  The SDK stamps priority from the manifest and actor with the unit.
- **Arbiter.** The service that orders commands to arbitrated entities,
  granting a **hold** (a lease per entity and aspect) to the winning
  band. See [Arbitrated mode](#arbitrated-mode).
- **Latch.** A commandable virtual entity: an automation-owned entity
  whose owner sets its state from the commands it receives.
- **Virtual entity.** An entity an automation owns; its value is
  computed, not read from a device.
- **Feed.** A device input wired to another entity's aspect, declared
  in the device's `[inputs]`. See [Device feeds](#device-feeds).
- **Source.** (1) In `[sources]`: a reading a computed value is derived
  from. See [Sources](#sources). (2) In a forecast key: the unit or
  provider claiming that future. See [Forecasts](#forecasts).
- **Forecast.** A series' future, published at `home/forecast/...`
  beside its present at `home/state/...`.
- **Live parameter.** A unit setting at `home/config/{unit}/{param}`,
  validated by the core on every write. Who may change it live is its
  `editable_by` tier: `owner` or `family`. See
  [Live parameters](#live-parameters).
- **Liveliness token.** What a unit declares on the bus once it can do
  its job; "up" means the token is present.
- **Health.** A unit's supervision status (`starting`, `running`,
  `backoff`, `open`, `stopped`). A **health event** is a unit's own
  report at `home/health/{unit}/event`; a **drop** is the event for
  input the unit refused.
- **Mirror.** The core's last-value cache, which a late joiner reads
  instead of waiting for the next publish. See
  [The last-value mirror](#the-last-value-mirror).
- **Plan.** The diff between the house repo and the running world,
  with its **tier**: `parameter-only`, `behavioral` or `structural`.
  **Apply** executes it. See [Plan and apply](#plan-and-apply).
- **Applied commit.** The house repo commit the running world was last
  applied from.
- **Discovery record.** What an adapter publishes about the devices its
  backend knows, bound or not. See [Discovery](#discovery).
- **Notable.** A reading the vocabulary marks as out of the ordinary;
  the dashboard lists it as a **deviation**.
- **Surface.** A way people or agents reach the house: the dashboard,
  the MCP server, notifications. See [Surfaces](#surfaces).

## Key space

Every key starts `home/{class}/`. The class list is fixed in
`src/keyspace.rs` (`CLASSES`); a manifest expression with any other
class is a plan error. Four classes are entity-addressed and share one
shape:

```
home/{class}/{room}/{entity}/{aspect}            state, cmd, arbiter
home/forecast/{room}/{entity}/{aspect}/{source}  forecast
```

The forecast `source` is required and state has none
([Sources](#sources)). Other classes take their own shape:

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

- **One room segment**, no floors or areas; a room is a field of the
  entity file. Entity names (the file stem) are unique across the house
  (`duplicate-entity-name`), so an entity is addressable without it.
- **Reserved words.** Entities with no place live in the pseudo-rooms
  `global` or `person`. These, `home` and every class name are
  reserved: a room may be a pseudo-room but no other reserved word
  (`reserved-room-name`), a zone none of them (`reserved-zone-name`).
  The unit name `system` is reserved for `home/meta/system/**`.
- **Every name is one key segment.** Unit, parameter, entity, room,
  zone and view names match `[A-Za-z0-9_.-]+` and are not `.` or `..`
  (`invalid-name`): anything else breaks the fixed shape (`/`), means
  something to Zenoh (`*`, `$`, `?`, `#`) or invites encoding
  surprises. The SDK's key builders (`homeostat.keys`) apply the rule
  at runtime; an adapter drops a device-chosen field name that fails
  it with `malformed-payload`.
- **Zones never appear in keys.** A zone is a named set of rooms in
  `zones.toml`; a zone in an expression's room slot expands to one
  expression per member room, at plan time and identically in the SDK,
  so a unit subscribes to what `plan` printed. A zone may not share a
  room's name or contain a pseudo-room; an empty zone is a warning.
- **Templates.** `{room}` and `{entity}` expand once per entity the
  unit binds ([Key expansion](#key-expansion)).

**Identity or space.** A subscription chooses: `home/state/kitchen/**`
is whatever is in the kitchen, `home/state/*/that_lamp/**` is that lamp
wherever it lives.

**Moving an entity** is an entity-file edit through plan and apply: the
owner's `files_hash` changes and the walk restarts it, and grants that
reached it by room are re-resolved. History survives, because a series
is keyed without its room, which is why `home/history` keys have no
room segment ([History and the recorder](#history-and-the-recorder)).

### Bus payload conventions

**JSON everywhere**, UTF-8, except in the core's meta space:
`manifest_hash` and `files_hash` are hex text, `manifest` is the raw
TOML, and `applied_commit` is the commit as text.

**State is one bare JSON scalar per aspect**: a boolean, a number or a
string, never wrapped. A composite reading is split into aspects (a
position is `lat` and `lon`), because the mirror, the recorder and the
dashboard handle one scalar per key, and a per-aspect key makes each
part's history free. An object, array or `null` on a state key is a
bug, which the recorder drops (`non-scalar`).

**No non-finite numbers.** JSON has no NaN or Infinity, but Python's
`json` writes them. The SDK's `put_json` drops such a value with a
`drop` event (`non-finite`), the recorder drops one from any other
publisher, and the dashboard refuses one, so no consumer has to guard.

**Times carry an offset**: clock payloads are local RFC3339
(`"2026-07-03T21:04:00+02:00"`), and the SDK refuses a naive forecast
timestamp. Recorder events and log lines use integer µs UTC.

**Commands are envelopes**, `{value, priority, actor, id}` on every
`home/cmd` and `home/arbiter` payload, `priority` one of `automation`,
`agent`, `family`, `manual` ([Cmd envelopes](#cmd-envelopes)).
Priority and actor are self-declared and checked only for shape
([Local-only access](#local-only-access)). An adapter's
`parse_command` drops a non-JSON payload with `malformed-payload`, and
one lacking `value` or a known `priority` with `invalid-command`.

**Commands travel as commands.** `put_json` sends `home/cmd` and
`home/arbiter` puts with congestion control `BLOCK` at priority
`INTERACTIVE_HIGH`. Zenoh's default, `DROP` at data priority, suits the
next temperature reading, not "unlock the door".

**Commands are puts, not queries.** The outcome, a readback on
`home/state`, arrives long after a query would close; who listens (the
owner's liveliness token) and a refusal or drop (events carrying
`cmd_id`) arrive anyway; and the recorder never sees a query. Queries
serve where one owner answers in full at once: parameter writes, apply.

**Queries.** A read is a GET without payload; a write is a GET with a
JSON payload, refused with an error reply (`{"error": "<message>"}`
from the config queryable). Mirror replies carry the value's age
([The last-value mirror](#the-last-value-mirror)).

**Documents.** A class that is not one scalar carries one JSON document
per key: health status, a discovery record array, the arbiter's
`{"schema": 1, "holds": [...]}`, a [forecast](#forecasts) issue
`{"schema": 1, "issued": ..., "points": [...]}`. Health events are
objects with a `kind` ([Health events](#health-events)).

## Units and manifests

A house is text. One manifest per unit under `units/`, one entity file
per entity in the entities dir of the unit that binds it, an optional
`zones.toml` and an optional `dashboard.toml`. Every file begins with
`schema = 1`; a file declaring another version is refused
(`unsupported-schema`) rather than half-read. The field-by-field
reference is [`docs/manifest.md`](manifest.md); this section covers
what the files mean and why.

### Unit kinds

Every running thing other than the core is a unit: one OS process, one
manifest, one liveliness token. The manifest's `[unit] kind` is one of
three, and decides which sections the manifest may carry
(`invalid-manifest` otherwise), which key classes it may publish, and
where it sorts among ties in the [apply walk](#the-apply-walk).

- **`adapter`** puts devices on the bus and is the only place a device
  dialect is spoken. It must carry `[discovery]` (how it reaches its
  backend) and `[entities]` (where its entity files live).
- **`automation`** regulates: it subscribes to state and publishes
  commands. It has no `[discovery]`, and may carry `[entities]` to bind
  entities with no device behind them
  ([Virtual sensors](#virtual-sensors),
  [Commandable virtual entities](#commandable-virtual-entities)).
- **`service`** is infrastructure with no entities: the recorder, the
  dashboard, the arbiter, the clock. It may carry `[discovery]` (the
  recorder's store is its endpoint) and may not use `{room}`/`{entity}`
  templates. Only a service may publish under `home/arbiter/`,
  `home/clock/` or `home/history/` ([Reserved classes](#reserved-classes)).

**The clock service** (`adapters/clock.py`) owns civil time. It
publishes `home/clock/minute` (local RFC3339 with offset) each minute
on the minute and `home/clock/date` at local midnight, and both at
startup, so a restarted subscriber never waits up to a minute. Its
timezone is an owner parameter: DST is handled in one place, and no
subscriber does naive time arithmetic.

The unit is the atom of authority, failure and change; how many rules
one unit hosts is its author's call
([Unit granularity](#unit-granularity-the-atom-is-the-unit)).
`[runtime]` says how the supervisor runs it
([The unit contract](#the-unit-contract)). A manifest declares **no
dependencies between units** (the bus decouples them, and the one
order that matters is derived from the [grant table](#the-grant-table)),
**no version** (the repo is the version) and **no health section**
(health is derived from liveliness).

**Entity files.** The entity is the resource, and its file is the sole
authority on its write policy: automations declare what they want to
publish, never exclusivity. The file stem is the entity's name. `room`
is the single source of spatial truth. `id` is the adapter-native
address, required on an adapter-owned entity and optional on an
automation-owned one. `[write_policy] owner` must name the unit whose
entities dir holds the file (`owner-mismatch`), so the binding is
stated in the file and its location must agree. `[naming]` (`sv`,
`en`, `aliases`) on units and entities labels the dashboard and voice.

### The manifest is the contract

The structs in `src/manifest.rs` are the manifest contract.
`deny_unknown_fields` makes a misspelled key an error, not a silently
ignored line, and their doc comments are the field descriptions. From
them come `homeostat schema [unit|entity|zones|dashboard]` (JSON
Schema, also the MCP `schema` tool), `homeostat schema --markdown`
([`docs/manifest.md`](manifest.md); a test refuses a stale copy) and
the [vocabulary table](#the-capability-vocabulary).

Every rule beyond the shape has a stable error code
(`error[<code>] <subject>: <message> (<file>)`). `CODES` in
`src/error.rs` gives each a paragraph on what the rule is and why,
read by `homeostat explain`, the MCP `explain` tool and every refused
plan; a test ties emitted codes and entries one to one. One source,
because a person or an agent must learn the contract without reading
Rust, and a second description would drift.

### What validation guarantees

`homeostat::check` is the plan-time pipeline every command starts
from: load, validate, expand key expressions, resolve grants, feeds and
sources. Errors accumulate across stages, and a file that fails to
parse is reported and skipped, so one bad file never hides the errors
in the others. `plan`, `apply` and `up` refuse a house with any error;
warnings refuse nothing. A house that passes guarantees:

- **Names** are key segments and none is a reserved word
  ([Rooms, entities and zones](#rooms-entities-and-zones)). Unit and
  entity names are unique, entity ids unique within one owner.
- **Shape per kind**; one owner per entity, which exists and holds
  the file; known capabilities, with a write mode wherever commands go.
- **Zones** whose members are rooms some entity is in, never
  pseudo-rooms, and whose names are not room names.
- **Parameters** whose default matches the type and its own constraint.
- **Keys** inside the key space, templates only where they expand,
  state only under bound entities, reserved classes respected and
  write policy satisfied
  ([Capabilities, grants and write policy](#capabilities-grants-and-write-policy)).
- **Feeds, sources and `dashboard.toml`** whose references resolve.

Validation does not guarantee runtime behaviour: the SDK keeps a unit
inside its declaration, but a process with its own bus session can
publish anything ([Security model](#security-model)).

### Key expansion

`src/expand.rs` expands every `[bus]` expression at plan time by the
rules in [Rooms, entities and zones](#rooms-entities-and-zones):
templates once per bound entity, a zone once per member room, cmd and
arbiter templates split by write mode, which is how
[arbitration](#arbitrated-mode) is enforced by structure. The result is
a derived entity registry, never mutated by a UI; `plan` prints it
([Plan and apply](#plan-and-apply)) and the SDK performs the same
expansion at runtime against the same files.

An expression that expands to nothing subscribes to nothing and reports
healthy, so it is never silent. A template in a unit with no
`[entities]` table is an error (`template-without-entities`); a binding
unit with no entities yet, or a zone with no rooms, is a house being
built up and a warning. A templated `home/cmd/` left empty because every
bound entity is arbitrated is correct and says nothing.

### Discovery

The `discovery` class carries an adapter's complete current view of its
periphery, bound or not: one JSON array at `home/discovery/{unit}`, the
whole inventory each time it changes. Each record carries `id` (the
exact value an entity file's `id` must use; only the adapter knows its
binding rule, so nobody guesses), whether and by which entity it is
bound, a best-effort `suggested` stanza in homeostat vocabulary, the raw
protocol descriptor, and on bound records the entity's
[aspect descriptor](#aspect-descriptors). The format is in
[`docs/adapters.md`](adapters.md), section 5.

- **One key, whole inventory.** Device ids may contain `/`, so a
  segment per device is a trap; a complete document makes departures
  trivial and matches the consumer, which filters `configured = false`.
- **The adapter suggests, the review decides.** A hard
  protocol-to-capability mapping makes unknown devices invisible;
  mapping on the agent side puts a per-protocol table in every agent.
- **The core stays thin**: it mirrors `home/discovery/*` and nothing
  else. **Opt-in.** **An unbound device is a fact, not a fault**, the
  normal condition of a house being configured.

Out of scope: rooms (no protocol knows them), actuating discovery
(permit-join carries authority) and inventory history (an array does
not fit the recorder's series). The workflow: read the record, write
entity files for unconfigured devices, `homeostat plan`, owner applies.

### The SDK's view of a unit

A unit reads its configuration from the files the core validated: the
supervisor starts it at the house root with `HOMEOSTAT_UNIT` set, and it
reads `units/{unit}.toml` and its entities dir. There is no core-to-unit
configuration protocol. `[discovery] endpoint` may reference `${VAR}`,
expanded by the adapter, because ports and credentials do not belong in
the repo; an unset variable is a startup error.

Adapters use `homeostat.house.load_adapter` and a `UnitSession`.
Automations use the Context, `homeostat.automation.context()`, which
gives exactly the surface the manifest declares: `ctx.subscribe`
(seeded from the [mirror](#the-last-value-mirror)), `ctx.params`
([Live parameters](#live-parameters)), `ctx.publish`,
`ctx.publish_forecast` ([Forecasts](#forecasts)), `ctx.restore`
([Restoring a unit's own last value](#restoring-a-units-own-last-value)),
`ctx.source_used`
([Which sources a computation actually used](#which-sources-a-computation-actually-used)),
`ctx.health_event`, `ctx.ready()` (the liveliness token, once the unit
can do its job) and `ctx.run()`.

**Binding names, not keys.** `subscribe` and `publish` take a binding
name from `[bus.subscribes]` or `[bus.publishes]`: the code says
`lights`, the manifest says which keys `lights` means, and changing
that is a manifest change `plan` shows. `ctx.publish(binding, value,
room=, entity=, aspect=)` puts to one concrete key: literal segments
are defaults, wildcard and template segments must be named, and an
uncovered key is refused, since a put on a `**` expression would hand
adapters an unparseable key. A `home/cmd/` publish is wrapped in a
[cmd envelope](#cmd-envelopes) at the declared
[band](#priority-bands).

## Capabilities, grants and write policy

Authority is declared in text and resolved at plan time. An entity
file says what the entity is (its capability) and how commands toward
it are governed (its write policy); a manifest says what a unit wants
to publish. The plan resolves every publish against the entities into
the grant table: the permission record the owner reviews and the
dependency graph the apply walk follows.

### The capability vocabulary

A capability says what kind of thing an entity is. The list is fixed in
the core (`CAPABILITIES` in `src/manifest.rs`): `binary_sensor`,
`burner`, `camera`, `climate`, `cover`, `light`, `lock`, `notifier`,
`person`, `presence`, `router`, `sensor`, `switch`; an unknown one is
`unknown-capability`. Each has a row in `VOCABULARY`, kept in step by a
test and rendered into [`docs/manifest.md`](manifest.md):

- **Base aspect**: the aspect commands target and the widget acts on
  (`on` for a light, `locked`, a climate's `setpoint`). A capability
  without one takes no commands, and its entities need no write mode.
- **Features and reserved aspects**: optional aspects an entity file
  may declare (`brightness`) and normalized readings the vocabulary
  names (`indoor_temperature` on `climate`).
- **Notable**: the reading that is a deviation on the dashboard's `Now`
  (a light on, a lock unlocked, a router's `wan = false`).

Every capability also has `available` ([Availability](#availability))
and `{aspect}_valid`, a boolean beside a reading the device itself may
stop trusting. Less obvious rows: `camera` has `motion`, media never
on the bus ([Cameras](#cameras)); `person` has `presence`, `lat`, `lon`,
`accuracy`, `battery`, `fixed_at` ([Map and people](#map-and-people));
`presence` takes `occupancy` or `presence`, as the protocol says;
`notifier` is in [Notifications](#notifications); `cover` is reserved.

**Why a fixed vocabulary.** Adapters speak homeostat vocabulary, never
their dialect's, so an automation, a widget, a deviation rule and a
grant written against `light` work for every adapter that binds a
light. Aspects outside the vocabulary pass through under their native
names, and an [aspect descriptor](#aspect-descriptors) labels them.

**How it grows.** By need: *the inputs an automation or an interlock
needs are vocabulary; thresholds are configuration.* A proposal built
against one case (a `mode` capability, a `heat_source` spanning a
burner and a heat pump) waits for a second.

**Presence and connectivity are house state; network metrics are
observability**, for the monitoring tooling beside homeostat, so there
is no `vpn` capability. Fusing sightings into "someone is home" is an
automation ([Virtual sensors](#virtual-sensors)). "I cannot see" is
never "nobody is home": an adapter that loses part of its view holds
those aspects stale and says so in a health event.

### Priority bands

Every cmd publish leaves at a band declared per publish in the manifest
and stamped by the SDK. Lowest to highest: `automation`, `agent`,
`family`, and `manual` (the dashboard and voice). No unit publishes at
`agent` or `family` today. `plan` reads a cmd publish with no
`priority` as `automation`, but the SDK refuses to send one
([Open questions](#open-questions)).

THE FAMILY ALWAYS WINS OVER AUTOMATIONS. Arbitration orders commands by
band, and the manual band is exempt from exclusive-write counting. An
automation declaring `manual` plans with a warning.

**No band laundering.** A unit that republishes a command re-stamps it
with its own band, so a "scene" automation relaying a family button
press would demote family intent below any arbiter hold. Group actions
fan out at the manual edge instead, and house modes are latches that
consumers read, never relays
([Commandable virtual entities](#commandable-virtual-entities)).

### Write modes

`[write_policy] mode` is `shared` (any granted writer; last write
wins), `exclusive` (at most one unit granted onto it below the manual
band, `exclusive-write-conflict`, counted per unit since authority is
per process) or `arbitrated` ([Arbitrated mode](#arbitrated-mode)). A
capability that takes commands must state it (`write-mode-required`),
so a light or a lock never inherits a policy silently; elsewhere an
absent mode reads as `shared` and governs nothing.

### The grant table

A grant is one publish resolved against the entities it reaches
(`src/grants.rs`):

- **Writer rows** come from a non-adapter's `home/cmd/` publish, which
  must name a capability (`publish-missing-capability`) and is granted
  onto every entity of it that its key covers, with the band and each
  entity's room, write mode and owner. One matching nothing is a
  warning.
- **Binding rows** come from a binding unit's `home/state/` or
  `home/forecast/` publish over its own entities.

Adapters' cmd subscriptions form no rows: adapters embody entities
rather than command them, and compromising one compromises exactly its
bound entities, which is irreducible. Because every bound entity sits
in its owner's binding row and keys are part of a row's identity,
moving an entity, flipping its write mode, changing its capability,
rebinding it, or widening a publish from `.../on` to `.../**` is a
grant delta, which makes a plan [structural](#tiers).

**The table is the permission record.** `plan` prints it (whole
offline, the delta against a live world), and the supervisor serves the
applied table at `home/meta/system/grants`, where the dashboard reads
which units drive which entities at which band.

**The table is the dependency graph.** An entity's owner runs before
the units granted onto it ([apply walk](#the-apply-walk)). Owners can
be automations (latches), so a cycle is possible, and it is refused
(`grant-cycle`).

**Enforcement is plan-time plus trust**, except that key expansion
denies adapters a direct path to arbitrated entities. The hardening is
a bus credential per unit, which the declarations already shape
([Security model](#security-model)). Command payloads have no separate
validation layer: adapters check type and bounds (`invalid-command`).

### Reserved classes

The SDK only checks that a key is inside a declared expression, so the
plan decides which classes a unit may declare at all
(`reserved-class-publish`); otherwise an automation could declare
`home/arbiter/**` and forge post-arbitration commands. `home/config/**`
and `home/meta/**` are the core's. `home/health/{unit}/...` and
`home/discovery/{unit}` sit under the publisher's own name.
`home/arbiter/`, `home/clock/` and `home/history/` each belong to one
service. A `home/state/` publish must name an entity the unit binds
(`state-publish-unbound`); a `home/forecast/` one an entity that exists
([Forecasts](#forecasts)). `home/hold/{unit}` is the arbiter's by
convention and is not checked.

### Cmd envelopes

Every payload on `home/cmd/**` and `home/arbiter/**` is an envelope:

```json
{"value": true, "priority": "automation", "actor": "evening_lights", "id": "9f2c1a07"}
```

- **`value`** is the command; **`priority`** is the
  [band](#priority-bands), stamped by the SDK from the manifest.
- **`actor`** is the publishing unit, which the recorder stores with
  every command for the audit trail.
- **`id`** is minted per command by the SDK. A command is a proposal
  that may be refused or dropped, and only a device readback says it
  took effect; the events that end one (`refuse`, and `drop` with
  `invalid-command`) echo it as `cmd_id`, so a publisher can tell which
  of two overlapping commands ended. Optional: a hand-rolled
  publisher's events report `null`.

Adapters drop a payload that is not an envelope with `invalid-command`.
Both classes travel with blocking congestion control at high priority,
so a congested link sheds a temperature reading, never "unlock the
door" ([Bus payload conventions](#bus-payload-conventions)).

### Arbitrated mode

High-stakes entities (locks, the heat pump, the burner) are
`arbitrated`. The arbiter service (`adapters/arbiter.py`) holds the
write token for each, and the bus structure makes it impossible to
skip:

- **Writers do not know.** Every writer publishes to
  `home/cmd/{room}/{entity}/{aspect}`. The arbiter subscribes to
  `home/cmd/**`, ignores non-arbitrated entities, and forwards a
  granted wish, envelope unchanged, to the same path under
  `home/arbiter/`.
- **Adapters cannot hear a wish**: their templated `home/cmd/`
  subscriptions expand only over non-arbitrated entities
  ([Key expansion](#key-expansion)). An arbitrated entity no
  `home/arbiter/` publish covers could never be commanded
  (`arbitrated-uncovered`).
- **The output is its own class**, so no subscription confuses a wish
  with a grant.

**Leases.** The token is a lease per (entity, aspect), because one
entity can carry orthogonal control dimensions (the family sets a heat
pump's `setpoint` while an automation drives its outdoor offset). A
wish with no lease in force, or at or above the holder's band, is
forwarded and takes or refreshes the lease for `hold_minutes` (a
family-editable arbiter parameter); a takeover from a strictly lower
band publishes `preempt`. A wish below the holder's band gets a
`refuse` naming the holder and echoing `cmd_id`. Expiry reopens the
aspect to automations, so a forgotten override heals itself. Events
land at `home/health/{arbiter unit}/event`.

**Holds are published as state**, because a browser opening mid-hold
asks "is this held now?", which events do not answer: one document at
`home/hold/{unit}`, mirrored by the core, `{schema, holds: [{room,
entity, aspect, priority, actor, since, until, refused}]}`, where
`refused` counts the wishes the hold has turned away.

- **One document, not a key per lease**: a restart publishes an empty
  list with no keys to clear, expiry needs one timer, and a reader sees
  a consistent set.
- **Enforcement runs on the monotonic clock; `until` is its wall-clock
  twin**, so no clock step can shorten or stretch a real hold.
- **Expiry is published.** Arbitration expires leases lazily, on the
  next wish, so a thread waits on the earliest deadline and
  republishes. Readers also drop an entry past its `until`.

There is no way to hand control back early: an equal or higher band
refreshes the lease, and the only exit is expiry
([Open questions](#open-questions)). Automation-owned entities cannot
be arbitrated (`virtual-entity-arbitrated`), and a
[fed input](#device-feeds) never rides the arbiter: one master leaves
nothing to arbitrate.

### Aspect descriptors

A parameter reaches the dashboard with a type, a constraint and an
`editable_by`, enough for a good control; a raw aspect has only a name
and a value. So **an adapter may describe an entity's aspects** in the
same vocabulary: a `{schema, groups, fields}` document whose fields
carry a label, a formatting `kind`, a group, optionally `valid` and
`notable`, and for a commandable aspect `command: {type, constraint,
step?, editable_by}`, the parameter fields verbatim. Firmware names get
labels, never schema. The contract is in
[`docs/adapters.md`](adapters.md), section 5.

- **It rides the discovery record** (the `aspects` member of a bound
  record), already declared, mirrored and per entity. Not `home/meta/`:
  that is core-owned and served by the supervisor, so a unit's publish
  there is invisible to a fresh reader. Not a new class: a second
  self-description for what discovery serves. The core knows nothing
  of descriptors.
- **The dashboard still owns every widget**
  ([Dashboard](#dashboard)); adapters never ship UI. An undescribed
  aspect is demoted to a collapsed diagnostics group, never hidden.
- **Commands widen by the rule that gates parameters**: the dashboard
  admits one the descriptor declares family-editable, within its
  constraint, once the grant table admits the capability. The adapter's
  bounds remain the enforcement. A knob an automation drives is owner
  tuning, not a family lever.
- **`notable`** makes a described boolean reading true a deviation.
  **`readback_s`** says how long the device takes to report a command
  back, which only the adapter knows.
- **A capability's own aspects** (`on`, `locked`) are described as
  readings only; their controls stay the dashboard's.

### Commandable virtual entities

House modes (day/night, "asleep") have no device, are flipped by the
dashboard, buttons and the clock, and are read by many lights. They
are ordinary automation-bound entities, and **the shape is the latch**:
the owner subscribes to `home/cmd/{room}/{entity}/**` over its own
entities, sets its own state when commanded, and never republishes
onward. Consumers read that state at their own bands.

- **Capability `switch`**, room `global` or a non-spatial group name.
- **Commandable means the owner listens**: a cmd grant onto an
  automation-owned entity needs the owner's covering `home/cmd/`
  subscription (`virtual-entity-commanded`).
- **`shared` or `exclusive`, never `arbitrated`.** A button press
  travels at the automation band yet is family intent, which
  arbitration would rank below the dashboard; with nothing to contend
  for, last write wins.
- **A latch survives its own restart**, adopting its value from the
  mirror or, after a core restart, the recorder
  ([Restoring a unit's own last value](#restoring-a-units-own-last-value)):
  the value is the family's decision, which nothing can recompute.

Rejected: an adapter holding modes in memory (an automation in an
adapter's clothes), a `mode` capability, a relay (band laundering).

### Burners and interlocks

**`burner`** is a deliberately small capability for a combustion heat
source: base `on` (the family lever), feature `power_level` (an enum
constraint the existing control renders), readings `flue_temperature`
and `boiler_temperature` in °C. Everything else passes through raw.

- **Not `switch` plus sensors**: "is it making heat" would be read from
  dialect fields, and per-aspect leases need `on` and `power_level` on
  one entity, so holding the burner off does not freeze an automation's
  power level. `power_level` is vocabulary for safety too: a flue
  cutout's threshold depends on it.
- **`on` reads back the device's run state**, never echoes the command,
  since start and stop may be momentary writes. No run-phase vocabulary
  until a second burner adapter shows what generalises.

**Interlocks stay the device's job; homeostat is not in the safety
path.** No band fits. At `automation` an interlock shares the band of
the loop it guards against and is taken over within a minute; at
`manual` it works and lies, attributing a cutout to the family in every
audit surface. And a hold is timed while an interlock is conditional:
refreshing it each sample makes a continuous writer whose death
silently releases the burner. A house-local cutout at the automation
band is welcome as belt and braces, but the house must not rely on it.

- **Rejected: a band above `manual`.** Whether safety outranks the
  family is a values decision, not a constant, and it inherits the
  timed hold anyway.
- **Deferred: an inhibit class**: a unit asserting a lockout on
  `(entity, aspect)` while a condition holds, refused at every band. It
  is the right shape (an interlock removes an option rather than
  winning an argument) and the design to pick up if interlocks recur.

## Plan and apply

There is no state file. Desired state is the repo; actual state is what
the running supervisor reports over the bus. `homeostat plan` diffs the
one against the other, and `homeostat apply` commands the supervisor to
make the world match, so drift between a state file and reality cannot
exist. **Rollback is git**: check out the previous commit and plan and
apply forward. Plan never reads arbitrary commits.

**How plan sees the world.** `plan --bus <endpoint>` (or
`HOMEOSTAT_BUS`) reads the core's queryables as an ordinary client:
each unit's applied manifest and hashes, the grant table and applied
commit under `home/meta/` ([Supervision](#supervision)), and the live
values under `home/config/*/*`. With no endpoint, plan runs offline
against an empty world, labelled as such: every unit is a create and
the whole grant table is printed, which is what a house repo's CI
wants. An endpoint that does not answer is a hard error, never a
silent empty world: "create everything" against a house that is merely
unreachable is how a home gets started twice.

**What plan prints**: units to create (command, bound entities,
parameters), destroy and restart (with the reason), parameter changes
(`~ evening_lights/off_time  live="21:30"  repo="23:00"`), manifest
refreshes, the expanded keys of every created unit and every unit
restarting on a manifest change (new key surface an approval must
show), grant changes (the whole table offline), feeds and sources,
warnings, and the tier. There is no per-subscriber match-set diff: an
entity move shows as grant changes, not as which subscriptions now
match differently.

### Tiers

Every plan has a tier, derived from its diff and never declared
(`derive_tier` in `src/plan.rs`):

- **Structural**: a unit created or destroyed, or any grant-table delta,
  which includes entity moves, rebindings, capability changes and
  write-mode flips ([The grant table](#the-grant-table)).
- **Behavioral**: otherwise, a unit restarted (its code, manifest or
  files changed).
- **Parameter-only**: otherwise. Live values reset to the repo and
  parameter-level manifest changes refresh in place; nothing restarts.

Because the tier is a function of the diff, no structural change
passes as a parameter edit. In the core the tier gates only the apply
lock, which a parameter-only apply skips; otherwise it is the
reviewer's cue, and a [pending plan](#pending-plans) is how a
structural plan waits for the owner. The core does not check who sends
an apply ([Local-only access](#local-only-access)), and `apply` does
not prompt: review happens at `plan`.

### Change detection

A unit is unchanged when two hashes match the world's:

- **`manifest_hash`**: sha256 of the manifest file.
- **`files_hash`**: sha256 over its other repo inputs: every token of
  its command that resolves to a file under the house root (`uv run
  units/foo.py` hashes the script), its own entity files, and
  `zones.toml` when one of its expressions expanded through a zone.
  Paths are hashed with content, so a rename is a change. Imports are
  not followed, and the script's `{script}.py.lock` is not an input
  ([Open questions](#open-questions)).

**`[unit] inputs = "house"`** makes every manifest, entity file,
`zones.toml` and `dashboard.toml` a unit's inputs. The dashboard is a
view over the whole house: an entity bound to another adapter changes
what it renders while changing none of its own files, and without the
declaration `apply` would succeed and leave the page wrong.

**A manifest-hash mismatch is classified semantically.** Both manifests
are compared with every parameter's `default`, `constraint` and
`editable_by` stripped. Equal (and files unchanged) is a parameter-level
change, a refresh with no restart: the rebuilt store enforces the new
constraints and the served meta manifest updates. Anything else,
including adding, removing or retyping a parameter, is behavioral,
because a running unit read its manifest at startup.

**Parameter diffs** compare each live value with its repo default, one
rule for a changed default and for live drift. An integer default on a
`float` parameter is canonicalized, or `5` against `5.0` would plan as
perpetual drift.

### The apply walk

**The supervisor executes apply.** The CLI sends HEAD to the core's
`home/meta/system/apply` queryable; the supervisor re-validates the
repo and derives its own diff, so the CLI's plan is a preview. The
supervisor owns the process table, breakers and health, so
restart-and-await-readiness composes with supervision instead of
racing it. Not a CLI-driven walk (remote per-unit stop and start, its
own lock); not a signal to re-read (no verification, no result).

**One apply at a time.** A second request while one runs is refused,
not queued; a parameter-only apply bypasses the lock, so a setpoint
commit never waits behind a structural walk. **The walk** follows
grant order, with ties and unconnected units by kind (adapter,
automation, service) and then name, so it is deterministic:

1. **Parameters.** The store is rebuilt and every changed value put,
   under the config write lock, so a racing parameter write cannot
   lose on the bus while the store keeps it.
2. **Refreshes** of parameter-level manifest changes are recorded.
3. **Removals**, in reverse grant order: dependents stop first.
4. **Creates and restarts**, owners first. Each unit is stopped,
   launched as a fresh supervision task and awaited: success is health
   `running`; failure is breaker `open`, `stopped`, or 60 s. A fresh
   task is a fresh breaker, so new code earns a fresh failure budget.

**Apply is per unit and rolling, not transactional.** A failure halts
the walk in place; the reply names each step's result, the unit it
halted at and the units not reached, and the CLI exits 1. Earlier
units keep their new incarnations, and neither the served grant table
nor `applied_commit` advances, so a re-run plans exactly the remaining
work. A supervisor shutting down mid-walk halts it the same way. No
automatic rollback: undoing would be a second walk that can fail the
same way, and git gives a forward path back.

**`applied_commit`** is HEAD, suffixed `-dirty` for uncommitted changes
outside `plans/`, published after a fully applied walk. It is recorded
only when the house root is a worktree's top level, so a fixture nested
in another repo never inherits that repo's HEAD. At `homeostat up` the
repo on disk is taken as applied, and `applied_commit` stays unset
until the first apply.

### Pending plans

`homeostat plan --save` writes `plans/pending/{id}.plan`: TOML with
`id`, `actor` (`--actor`, default `owner`), `created`, `base_commit`,
`tier`, and the rendered plan as a literal string, readable on a phone.
It needs the live world and a git worktree, and refuses when there is
nothing to save.

`homeostat apply --plan <file>` refuses when `base_commit` is not HEAD
as it reads now, `-dirty` included, so a pending plan invalidates
itself when the repo moves past it; otherwise it recomputes the plan
fresh like a plain `apply`. The file is a review artifact, not an
execution script. `plans/` does not count toward `-dirty`, and applying
a plan does not remove it.

## Supervision

`homeostat up` validates the house, opens the bus as a router and
brings up the core's queryables (parameters, health, meta, mirrors,
apply) before any unit spawns, so a unit's first read finds them. Then
one supervision task per unit (`src/supervisor/`) owns its process,
watches its liveliness token, applies its restart policy and publishes
its health. The supervisor also publishes each applied unit's
`manifest_hash`, `files_hash` and `manifest` and the grant table, and
serves them with `applied_commit` and `about` from `home/meta/**`, the
live world `plan` diffs against ([Change detection](#change-detection)).

### The unit contract

What the supervisor guarantees at spawn (`src/supervisor/process.rs`):

- **No shell.** `runtime.command` is split on whitespace and exec'd
  directly, with `PATH` lookup and relative paths resolved against the
  house root, which is the unit's working directory. Stdin is
  `/dev/null`.
- **Its own process group**, and on Linux `PR_SET_PDEATHSIG(SIGKILL)`,
  so even a SIGKILLed supervisor leaves no unit running.
- **A filtered environment**: `HOMEOSTAT_UNIT` (its name),
  `HOMEOSTAT_BUS` (the endpoint, e.g. `tcp/127.0.0.1:7447`), a fixed
  base set from the supervisor's environment (`PATH`, `HOME`, `USER`,
  `LOGNAME`, `SHELL`, `LANG`, `LANGUAGE`, `TZ`, `TMPDIR`, `TERM`,
  `REQUESTS_CA_BUNDLE` and anything prefixed `LC_`, `XDG_`, `UV_`,
  `PYTHON` or `SSL_CERT_`), and the variables its manifest names in
  `runtime.env`, by exact name. Nothing else, so a secret handed to the
  supervisor for one unit never reaches another.
- **Captured output**, re-emitted on the supervisor's streams prefixed
  `[{unit}] ` and kept in a 500-line ring at `home/meta/{unit}/log`
  ([Logs and the audit trail](#logs-and-the-audit-trail)).

What every unit owes back:

- **Connect as a client** to `HOMEOSTAT_BUS`, scouting off. Zenoh peers
  do not route between clients, so the supervisor's router is the hub.
- **Declare a liveliness token** at `home/health/{unit}/alive` once it
  can do its job (`UnitSession.ready()`). The token, not the PID, means
  "up": it disappears with the session, whatever the process does.
- **Exit on SIGTERM** within `runtime.shutdown_grace_s` (default 5 s).

### Resolving `uv run`

For a command `uv run [flags] <script.py> [args]`, the supervisor runs
`uv sync --script` and `uv python find --script` to completion, then
execs the environment's interpreter on the script, so the interpreter
is the group leader and direct holder of `PR_SET_PDEATHSIG`. Any other
command is left alone.

- **Why.** `uv run` otherwise stays alive as an idle parent holding
  4 MB warm to 25-55 MB after a fresh resolve, and keeps the
  interpreter out of reach of `PR_SET_PDEATHSIG`, which reaches only
  the direct child.
- **The manifest still says `uv run`.** The PEP 723 block stays the
  single authority on dependencies, covered by `files_hash`. Every
  start resolves again, so a restart after an SDK bump picks it up.
- **Best effort.** If uv cannot resolve the script (no PEP 723 block, a
  broken dependency, no network), or the interpreter path would not
  survive whitespace tokenizing, the original command runs unchanged
  and fails exactly as `uv run` would.

### Health

The supervisor publishes JSON at `home/health/{unit}` on every
transition and serves it from a queryable there; nothing republishes.

```json
{"status": "backoff", "pid": null, "restarts": 2, "backoff_ms": 400, "last_exit_code": 1}
```

`starting` (spawned, token not yet seen or lost while the process
lives), `running` (token present), `backoff` (exited, restart due in
`backoff_ms`, present only here), `open` (breaker open, no more
restarts), `stopped` (policy `never`, a clean exit under `on-failure`,
or shutdown).

`restarts` counts from the start of the unit's supervision task: at
supervisor start, or at the last apply that restarted it. Each
incarnation gets a fresh liveliness subscriber, so a token event left
from the previous one cannot mark the next `running` early.

### Restart policy, backoff and the breaker

`runtime.restart` says which exits count: `always`, `on-failure`
(non-zero or a signal) or `never`. Restarts then follow one arithmetic
(`src/supervisor/backoff.rs`): the first waits 100 ms and each further
consecutive quick exit doubles it, capped at 30 s; a run lasting 5 s
resets the count; the fifth consecutive quick exit opens the breaker
(so the waits are 100, 200, 400 and 800 ms). Any quick exit counts,
clean or not, since a clean-exit loop is as much a crash loop as a
panic, and a failed spawn counts as an exit.

An open breaker stays open until the supervisor restarts or an apply
restarts the unit with a fresh task and breaker, so new code earns a
fresh failure budget ([The apply walk](#the-apply-walk)).

### Termination and sweeping

- **Stopping a unit** (shutdown, or an apply step) sends SIGTERM to its
  whole process group, waits up to the grace for the group, not just
  the leader, then SIGKILLs the group.
- **When a leader exits on its own**, the supervisor SIGKILLs the rest
  of its group before restarting, because a descendant left behind
  could keep the liveliness token alive into the next incarnation.
- **Supervisor shutdown** on SIGTERM or SIGINT stops every unit in
  parallel and reports each `stopped`. The handlers are installed
  before the bus opens, so a signal during startup is graceful too.
- **Global reaping** is tini's, PID 1 in the container image.

## Live parameters

A parameter is a value a running unit reads and someone may change
without a restart: an off time, a hold duration, a poll interval. A
unit declares each under `[params.<name>]` with a `type` (`bool`,
`int`, `float`, `string`, `time`), a `default`, an optional
`constraint` and an optional `editable_by` (`owner` or `family`). The
current value lives at `home/config/{unit}/{param}`. The clock's
timezone, the arbiter's `hold_minutes` and an adapter's poll interval
ride this path like any automation's setpoint.

The constraint language stays minimal: `min`/`max` for numbers,
`after`/`before` for times (`"HH:MM"`, a window that may span
midnight), `enum` for strings. A parameter needing more is
`editable_by = "owner"`, changed by someone who can read the code.

### The write path

Only the core ever puts on `home/config/**`. At startup it seeds every
parameter from its manifest default and declares one queryable on
`home/config/*/*` (`src/config.rs`):

- **A GET without payload is a read** of what the selector covers.
- **A GET with payload is a write request** for one key. The core
  checks the JSON value against the manifest's type and constraint.
  Accepted, it is stored, put on the key (subscribers see it at once)
  and echoed in an ok reply. Rejected, the reply names the violation,
  nothing is put, and the old value stands.

A write is a query rather than a put because one owner, the core,
answers it in full while the query is open: the writer learns
synchronously whether the value is in force. A plain put would bypass
validation. Not the Zenoh storage plugin: a passive mirror cannot
refuse an out-of-constraint write
([The last-value mirror](#the-last-value-mirror)).

A write and its put are one ordered step under the lock apply's
parameter step also takes ([The apply walk](#the-apply-walk)), so the
store and the bus never disagree. An integer written to a `float`
parameter is stored as the float it means, as the repo default is
read, so the two never differ by representation alone.

### Repo and live values

The repo is the system of record; the bus value is a live view of it.

- **A live edit survives a unit restart**, because the core holds it,
  but not a supervisor restart: the store re-seeds from defaults.
- **Plan shows drift and apply resets it.** Every live value that
  differs from its default is listed, and every apply sets live values
  to the repo's ([Change detection](#change-detection)).
- **Making a live edit durable** means committing it as the default:
  a parameter-only plan, with no restart and no apply lock.
- **Changing a constraint or `editable_by`** refreshes the unit in
  place; adding, removing or retyping a parameter restarts it.

A default outside its own constraint is `invalid-default`, so the repo
path is checked as strictly as the live one.

### Who may write

`editable_by` says who may change a parameter live. The dashboard
offers `family` parameters as setpoints, shows `owner` ones read-only
and writes nothing else. The core's queryable does not know who is
asking, so any bus client may write any parameter within its
constraint: reaching the bus is the boundary
([Security model](#security-model), [Open questions](#open-questions)).

### In the SDK

A unit follows its own `home/config/{unit}/*` implicitly, by subscribe,
then get, merge ([The last-value mirror](#the-last-value-mirror)).

- **Automations** read `ctx.params.<name>`, the typed current value
  (`time` as `datetime.time`); an undeclared name raises.
- **Adapters and services** use `LiveParams` (`homeostat.params`),
  with defaults of their own so a manifest may omit a parameter; it
  tracks finite numeric values only.
- **Writers** (the dashboard) call `UnitSession.write_config`, which
  raises `ConfigWriteError` with the core's message on refusal.

## State, history and forecasts

Three stores answer three questions: the core's last-value mirror says
what is true now, the recorder what was true and what was predicted,
and forecasts what a source claims will be true. All three are read
over the bus, so no consumer holds a database handle or a credential,
and each store stays private to its owner.

### The last-value mirror

The core keeps an in-memory last-value cache in the supervisor process
(`src/supervisor/mod.rs`, `mirror`). For each mirrored key space it
declares a subscriber and a queryable on the same expression: a put
replaces the key's entry, a delete removes it, and a GET replies once
per matching key with the last payload, byte for byte. Mirrored:
`home/state/**`, `home/forecast/**`, `home/clock/*`,
`home/discovery/*` and `home/hold/*`; `home/config/*/*` and
`home/health/*` have last-value queryables of their own
([Live parameters](#live-parameters), [Supervision](#supervision)).
All are up before any unit spawns, so a unit's first get finds them.

- **Every reply carries the value's age**, read off the mirror's
  monotonic clock rather than a wall-clock stamp (`get_json_aged`; see
  [Bus payload conventions](#bus-payload-conventions)).
- **The read pattern everywhere is subscribe, then get, merge**.
  `ctx.subscribe` does it for every binding, delivering each key's
  catch-up, with its age, before a live sample for that key
  ([Staleness](#staleness)).
- **The mirror never inspects a payload and expires nothing.** It
  cannot know a producer's cadence, so whether a value is too old is
  the consumer's question ([Availability](#availability)).
- **It is not durable.** A core restart, which every upgrade is,
  empties it; a decision nobody can recompute is restored from history
  ([Restoring a unit's own last value](#restoring-a-units-own-last-value)).

### History and the recorder

The recorder (`adapters/recorder.py`) is a generic service unit, one
per house ([Reserved classes](#reserved-classes)), writing one SQLite
file named by its `[discovery]` endpoint (`sqlite:<path>`, relative to
the house root, `${VAR}` expanded). It is not a bus mirror: payloads
are decoded and typed on the way in, and anything that fails leaves a
`drop` health event, never a row of garbage.

**SQLite, in production and in tests.** A home produces well under ten
samples a second, which an indexed SQLite file absorbs for years, CI
runs the identical engine with nothing beyond `cargo test`, and every
read goes over the bus, so outgrowing it would change one unit. Not
QuestDB or TimescaleDB: a permanent JVM or Postgres cluster outside the
unit model. Not DuckDB: columnar, weak at single-row inserts, and
single-process; it can `ATTACH` the store or an archive read-only for
analysis instead. Not a pluggable backend: tests and production would
diverge. Tiering [archives](#archives) to Parquet (about 18x smaller,
for a ~50 MB dependency and a second read engine) is held in reserve.

**What is recorded** is whatever the manifest subscribes. The shipped
manifest takes `home/state/**` and `home/cmd/**` into `samples` (a
command as its envelope's `value`), `home/forecast/**` into
`forecasts`, and into `events` the raw payloads of `home/health/**`,
`home/config/**` (only accepted writes are put there) and every command
envelope ([Logs and the audit trail](#logs-and-the-audit-trail)). Not
`home/clock/**` (a derivable row a minute, forever), `home/meta/**`,
`home/discovery/*`, `home/hold/*`, liveliness tokens or
`home/history/**`. The schema is `init_store()`'s, versioned by
`PRAGMA user_version` and migrated in place.

- **Series identity is `(class, entity, aspect, source)`; the room is
  a per-row tag**, so an entity that moves is one continuous series.
  `source` is `''` except for forecasts, not NULL, because SQLite
  treats NULLs as distinct in a unique index. Interned names keep a
  `WITHOUT ROWID` sample row near 25 bytes, against about 113 as text.
- **Each series carries its own tally** (rows, oldest, newest), or
  `stats` would scan a store growing with the problem it diagnoses and
  `ctx.restore`, which polls it, would time out.
- **The timestamp is recorder receive time** (µs, UTC), assigned before
  any buffering, so an outage never distorts history. Zenoh stamps are
  optional for client sessions, and one clock beats mixed provenance.
  Two samples for one series in one microsecond collide; the later is
  dropped.
- **Repeats are kept.** A republished value is still a sighting;
  dropping repeats would fix a republishing device but not a jittering
  float or an honest 1 Hz sensor. [Retention](#retention) and
  [archives](#archives) bound volume, and reads collapse repeats.
- **Only scalars** ([Bus payload conventions](#bus-payload-conventions));
  anything else is dropped (`non-scalar`, `non-finite`).
- `auto_vacuum = INCREMENTAL` (settable only before the first page)
  lets retention return pages; WAL keeps the writer and readers from
  blocking each other.

**The writer.** Subscriber callbacks stamp, type and enqueue; one
writer thread commits one transaction per flush on a connection opened
per flush, so no long-lived handle holds stale permissions or a
deleted inode. Reads open their own read-only connections. The store
must open before `ready()`, so a recorder without one shows as backoff.

- **Outage** (disk full, permissions, a dying SD card): a failed flush
  keeps the batch in a 10,000-row drop-oldest buffer (recent state is
  worth more), retried on new samples and every second. One
  `backend-outage` per down transition; on recovery the rows land with
  their original stamps and `backend-restored` reports what was
  flushed and dropped.
- **Poison rows** are bad data, not an outage: the batch is replayed a
  statement at a time, the refused row leaves a `drop`
  (`integrity-error`), and the rest commits.
- **Integrity check**: SQLite has no page checksums by default, so a
  separate thread runs `PRAGMA integrity_check` read-only every
  `integrity_check_hours` (owner, default 24, 0 disables; first run an
  interval after start, so a restart loop never hammers a large file),
  reporting `integrity-ok` or `integrity-failed` for the owner to act.

**Catch-up from the mirror.** A recorder subscribing at start misses
what was published just before, so a rarely-changing aspect could read
as "never published". After subscribing it enqueues the mirror's
`home/state/**` values it has not seen live, stamped at the value's own
time (now less its age), skipping a series with a row at or after that
time, within half a second. Not a start order putting the recorder
first: that misses a recorder restart, and is a dependency edge between
units. While the recorder is down there is a gap; nothing replays it.

### Read path

One queryable at `home/history/**`, declared under the recorder's
`[bus.publishes]` so the plan shows the read surface. Parameters use
Zenoh's `;` separator with no URL decoding, so an offset's `+` stays
literal. Keys are entity-first with no room slot.

- **Samples**:
  `home/history/{state|cmd}/{entity}/{aspect}?from=..;to=..;limit=..`
  (RFC3339 with offset), one reply per matching series, an array of
  `{ts, room, value}`. Two exclusive folds apply to the whole window
  before `limit`: `bucket=<seconds>` (a number's mean with `min` and
  `max`, otherwise the last value; over 10,000 buckets is refused) and
  `changes=1` (the rows where the value changed). Without them a
  chart's span would depend on publish rate; the page knows its width
  and the recorder the rows, so downsampling is the recorder's.
- **Forecasts**: `home/history/forecast/{entity}/{aspect}/{source}`,
  in issues in the wire's shape: `at=` (default now) is the latest
  issue at or before that instant, `valid_from=..;valid_to=..` every
  issue that spoke about the window, with only its overlapping points,
  which is what verification reads. `limit` counts issues.
- **Events**: `home/history/events?key=..;from=..;to=..;limit=..`,
  `key` a key expression and bounds integer microseconds.
- **Stats**: `home/history/stats`, the size and span of the store,
  each series, the events table and each archive, from tallies, never
  scans: choosing a retention window means knowing which series fills
  the file, and a host may lack `sqlite3`.

Every path clamps `limit` to 10,000, keeps the newest, and replies
oldest first. Anything malformed or unexpected gets an error reply
(the latter with a `query-failed` event): a callback that raises sends
no reply, indistinguishable from "nothing recorded", the one wrong
answer a history API must never give. Zenoh runs a queryable's
callback serially, so a slow answer delays every query behind it.

### Retention

Three owner windows in days, `retain_samples_days`,
`retain_forecasts_days` and `retain_events_days` (events, the audit
trail, are worth keeping longest), default 0, forever, so no upgrade
silently deletes history. Forecasts go by issue time, since superseded
issues are what grows. The writer thread purges hourly and on a window
change, per series in short primary-key range deletes, then `PRAGMA
incremental_vacuum`. One `purge` event per purge that deleted anything
(an empty one is silent); `purge-failed` is retried an hour later.

Retention is the only operation that deletes from the store, and it
makes noise visible: an adapter publishing every poll rather than on
change fills the file, and `stats` names the series. No downsampling:
a roll-up that deleted its source rows could not be additive, and an
analytical layer over `ATTACH` can roll up without deleting.

### Archives

Archiving moves rows out of the store without deleting them, so the
store stays one window deep and every observation is kept. With
`archive_after_months` above 0 (owner, default 0 = never), each month
that closed longer ago than that is sealed into
`archive/<store>-YYYY-MM.db`, one month per pass, on the writer thread
after the hourly purge (so a row past its window is deleted, not
archived). Events: `archive`, `archive-failed`. `home/history/**`
answers from the store alone: every query the system makes lies within
a month or two, and archives are for people and tools.

- **A plain, uncompressed SQLite file with the store's schema** and
  ids, readable as the store is; an archive that must be unpacked
  first is one nobody opens.
- **Sealed once, never written again**: written under a temporary
  name, verified, fsynced, renamed, recorded with its SHA-256 and made
  read-only. A crash mid-seal loses nothing: the next pass seals a file
  that is present and discards an attempt whose file is not, since
  nothing was pruned. Late rows go into `.2`, `.3`. The checksum is
  the whole of a later check, and a backup's diff is the current window.
- **Only what a sealed file holds leaves the store**, matched on the
  whole row. **Each series' newest sample and forecast issue stay**
  until overtaken, because `ctx.restore`, the seed and every
  latest-value read look in the store.
- **`retain_archives_months`** (owner, default 0 = forever) drops whole
  files, as its own setting so a store window never deletes archives.
  It deletes the file before its record, so a crash leaves work the
  next pass finishes (`archive-dropped`).
- **Settings that undercut each other** (a `retain_*_days` window under
  `archive_after_months` + 1 months, archives kept no longer than
  archiving waits) are allowed, and reported once per change as
  `archive-misconfigured`.

### Forecasts

Model-predictive control is a standing assumption, and it needs
forecasts. Homeostat provides only the mechanism (a place to put a
forecast, a store that keeps every issue, a chart that draws them);
producers and controllers are house behaviour ([Repo split](#repo-split)).
A forecast is **a source's claim about a series' future values**, so a
controller's planned trajectory needs no second class; whether a claim
is a prediction or an intent follows from the aspect ([Sources](#sources)).

- **Keyed like the series it extends, plus who says so**
  (`home/forecast/{room}/{entity}/{aspect}/{source}`), sharing its
  descriptor and chart axis; several sources may speak about one aspect.
  The source slot is required, never optional: with two key shapes, no
  one wildcard expression would match every opinion about a series.
- **The entity must exist; the publisher need not bind it**
  (`forecast-publish-unbound`): a weather service or a controller is
  routinely not the binder. Two units whose publishes can land on one
  key are `forecast-publish-conflict` (slot by slot, a wildcard
  colliding with any literal), since the mirror keeps one document.
- **Two time coordinates are the whole difference from state**, when it
  was said and when it is about, so even a one-point forecast cannot
  ride `samples`, where two issues about one instant would collide. The
  payload, one issue atomically, is `{schema: 1, issued, points:
  [{t, v, d?}]}` with offset timestamps (an hour-wrong forecast is
  worse than none).
  - **Irregular points**, as the source said them, because resampling
    is not single-valued (a price holds, a temperature interpolates).
    The consumer names the rule: `Forecast.at(when, mode, max_gap_s)`,
    `step` or `linear` with no default, and `resample()` give `None`
    across a gap, so a controller can refuse rather than optimise
    against invention.
  - **`d` is a point's extent in seconds**, `[t, t + d)`: otherwise an
    accumulation reads as a spike, and a horizon's last held value has
    no length. Reading an interval point `linear` raises.
  - **`issued` is required and is the whole staleness story**: the
    consumer applies its own maximum age (`Forecast.age_s()`).
  - **`put_forecast` refuses a bad payload** (over 2048 points,
    non-finite, duplicate instants) with a `drop` (`invalid-forecast`).
- **Mirrored by the core**, or a consumer starting at midday would be
  blind to a day-ahead curve until tomorrow. **Producers publish their
  current forecast at startup**, because a core restart empties it.
- **Stored per point, every issue kept**, since checking superseded
  issues against the outcome is why forecasts are recorded. Rows carry
  the producer's `issued`, never receipt, so a replayed issue cannot
  pose as fresher; an issue is accepted or refused whole.
  `valid_end` is stored, since a last window has no successor to derive
  it from. Not a widened `samples`: a nullable `issued_ts` cannot ride
  a `WITHOUT ROWID` key, folds would average across issues, and
  retention would need a class-conditional purge. Rows older than the
  source segment carry the reserved source `_unknown`.
- **A quiet producer is never diagnosed from its document's age**: a
  crashed one is supervision's, and a failing upstream is the running
  producer's health event. The [dashboard](#dashboard) draws forecasts
  under its own staleness policy.
- **Not representable**: uncertainty and corrections to the past
  ([Open questions](#open-questions)). Percentiles as separate sources
  are the tempting misuse: one issue's percentiles are one claim.

### Restoring a unit's own last value

The mirror survives a unit restart, not a core restart. After an
upgrade a latch would come back at its code default, disagreeing with
what the family set, and a fusion needing every input would be blind
until its slowest source publishes. **The right behaviour differs by
kind of state, and only the unit knows which it holds**, so restoring
is a call the unit makes, never framework behaviour:

| unit | on start | why |
|---|---|---|
| one-way sender adapter | publish `false` | the held value was its own construct ([One-way senders](#one-way-senders)) |
| fusion | recompute, seeded from the mirror | derived, and a stale input is dangerous |
| latch | restore what was last published | a person decided it; age is irrelevant |

- **`ctx.restore(binding, room=, entity=, aspect=, timeout_s=30)`
  returns `(value, age_s)` or `None`**, from the series' newest row in
  the recorder, the only record that a decision was made. It reads only
  the unit's own published state keys, resolved through the binding as
  `publish` is: somebody else's state is a different and worse thing,
  and a command is an event with nothing to restore.
- **The age comes with the value**, which a latch ignores and a fusion
  checks; a bare value would make the dangerous case the easy one.
- **`None`, never an exception**, when there is no recorder, no rows,
  or a store error (`restore-failed`), so a house without a recorder
  starts on code defaults.
- **It waits for the recorder, because there is no start order**,
  polling `home/history/stats` (an empty series gets no reply) until
  the timeout, unless no unit publishes under `home/history/`. Call it
  before `ready()`. The recorder answers serially, so a get it took is
  waited out rather than re-asked into its queue; only an unserved get
  is repeated. Reads are not a declared bus surface; what constrains
  `restore` is the unit's own publish bindings.

Not persisting unit state in the SDK or supervisor: the same framework
guess, and it would resurrect a one-way sender's expired motion event.
Not a file beside the unit: a second store with its own retention,
backup and corruption story.

## Derived values

A value the house computes rather than measures (a fused temperature,
"someone is home") is ordinary state on an ordinary entity, bound by
the automation that computes it. Consumers never learn whether a value
was measured or derived, deliberately, just as the bus does not leak
adapter-native vocabulary; provenance is visible where structure
lives, in the entity file's owner and in the plan.

### Virtual sensors

A virtual sensor is an entity whose binding unit is an automation:
**exactly one unit binds each entity**, adapter or automation. Presence
fusion (router sightings, phones and motion into "someone is home") is
the typical one.

- **Mechanics.** An automation may carry `[entities]`
  ([Unit kinds](#unit-kinds)), expanded as an adapter's are, and their
  files may omit `id`: a computed value has no device-native address.
- **The entity file is what makes everything downstream free**: the
  recorder, the dashboard widget, the mirror and `read_state`, the
  notable vocabulary and voice grammar all come from the entity
  registry. A free-form state key would be recorded and invisible to
  every generated surface, so a state publish must fall under an
  entity the unit binds (`state-publish-unbound`); forecasts are held
  only to the entity existing ([Forecasts](#forecasts)).
- **Read-only unless the owner listens**, which makes it a latch
  ([Commandable virtual entities](#commandable-virtual-entities)).
  Chains need no ordering: a late joiner reads the mirror.
- **Room.** A fusion across rooms lives in `global`: "downstairs" is a
  zone, and zones never appear in keys. A virtual sensor honestly about
  one room uses that room. Where it appears on the dashboard is
  `dashboard.toml`'s say, never a second spatial truth.
- **Staleness is the producer's obligation**, a norm rather than
  machinery, since the core cannot know which inputs a fusion needs. A
  fusion of stale inputs goes stale rather than confidently
  republishing ([Staleness](#staleness)), and reports which inputs it
  used ([below](#which-sources-a-computation-actually-used)).

Not a `derived` key class: it fragments the vocabulary every consumer
keys on. Not a generic fusion adapter with rules: which sensors and
weights is house behaviour, and a rule language for it is a DSL. Not
fusion across adapters inside one: an adapter may derive on its own
bound entities and no further.

### Device feeds

A heat pump's "actual indoor temperature" input is not a command: it
is a continuous signal with one master, where the failure that matters
is staleness, not contention. A **feed** wires a device input to one
source aspect. Which inputs are fed is a per-house decision, so the
wiring lives in the fed entity's file:

```toml
[inputs]
indoor_temperature_actual = { entity = "indoor_temperature", aspect = "temperature" }
```

- **The reference is entity and aspect**, not a bus key or a unit:
  entities are the identity layer keys derive from, and a device
  consumes one signal. The plan resolves it to a state key and prints
  the edge under `Feeds:`.
- **Plan-time checks** (`resolve_feeds` in `src/grants.rs`): the source
  entity exists (`input-unknown-entity`), an automation-owned source
  publishes the aspect (`input-unpublished-aspect`), and the fed entity
  is a device (`virtual-entity-fed`). Any owner's aspect is feedable.
- **The adapter is the authority on input names**, which are dialect
  knowledge: it refuses to start, visibly, on an input it does not
  know, and drops the corresponding command aspect for an entity that
  feeds it, so the input keeps one master.
- **Staleness is the device's.** The adapter forwards source samples
  while the source's `available` is not false; then the device's own
  validity window expires the term and it falls back on its own
  control. No adapter-side timeout or refresh: a transition-only source
  quieter than the window is house tuning.
- **A fed value must not outlive its source in a broker**, which would
  serve it across reconnects: it is published unretained, and on source
  loss the adapter clears any retained copy and reports
  `feed-source-lost`.
- **One subscriber per source entity** (`.../{entity}/*`), because
  Zenoh orders samples only within a subscriber, and a value overtaking
  the `available = true` before it would be dropped. A value arriving
  while the source is unavailable leaves one `drop`
  (`feed-source-unavailable`) per outage.
- **Feeds are not walk-order edges.** A control loop that reads a
  device and feeds a term back is legitimately cyclic, so the
  [apply walk](#the-apply-walk) orders by grants only.
- **One computation may straddle both**, one output fed and another
  commanded, with no guarantee they land together; an automation that
  needs that holds the command's lease and feeds against it.

Not a fifth command aspect marked non-arbitrated: it misdescribes
sensor feedback and puts it in the grant table next to setpoints. Not
a new grant kind: machinery for what an entity-file reference
expresses. Not an adapter subscribing a raw bus key: that bypasses the
identity layer.

### Sources

The key space names the thing that reports, never the thing reported
on. For a lamp the two are one object; several sensors and a weather
service with opinions about the outdoor air is where they come apart.

- **Physical versus virtual is invisible** and never shapes a key.
- **Commandable versus read-only is a property of an aspect, not an
  entity.** Leases are per aspect and descriptors carry `command` per
  field ([Aspect descriptors](#aspect-descriptors)): a heat pump's
  `feed_temperature` is a reading, `feed_temperature_target` takes
  commands. Asking "sensor or control?" per entity is what makes a
  second noun for the subject look necessary.
- **A plan and a prediction are one class, and which is derivable**: a
  forecast for a commandable aspect is a plan, for a read-only one a
  prediction, so no `kind` field. Divergence from the outcome measures
  a prediction's accuracy but a plan's authority (revised, arbitrated
  away, clamped), so an error metric must not average the two.
- **The source segment is on forecasts only.** Two outdoor sensors are
  two entities, since a sensor is in the house with a room and a
  failure mode; a weather service is not, and an entity for it would
  name subject and provenance in one string. Competing estimators of
  one state value are separate entities until that stops being rare.

**Declared sources on a derived entity.** A computed entity may declare
what it is computed from, in a feed's reference shape:

```toml
[sources.kitchen]
entity = "kitchen_temp"
aspect = "temperature"
note = "south-facing; reads high on a sunny afternoon"
precision = 0.5
```

- **Declared, not inferred**: a fusion subscribes many things for many
  reasons, and no subscription says which inputs feed which aspect.
- **Checked like a feed** (`resolve_sources`: `source-unknown-entity`,
  `source-unpublished-aspect`), plus a plan warning for a declared
  source the owner does not subscribe, so the declaration is a checked
  fact. The plan prints the edges under `Sources:`.
- **`note` and `precision` are the contributor's own caveats** (it sits
  in the sun; it is merely coarse, not disagreeing), shown beside it in
  the overlay. Kind and unit stay on the aspect descriptor, a contract
  every source is held to.
- **Not `[inputs]`, though the shape matches.** A feed carries a
  runtime contract and retires a command aspect, which would collide on
  a commandable virtual entity that also declares sources.
- **Nothing in the store changes**: each contributor is its own
  series, drawn by the overlay's `sources` view ([Dashboard](#dashboard))
  as its own line, never a band, whose edge would trace a path no
  sensor took.

Not a second noun for "the thing reported on", with its own key shape
and a canonical-selection rule: the bullets above, `[sources]` and
optional `id` on automation-owned entities cover its cases.

### Which sources a computation actually used

Declared sources say what may contribute; a fusion that drops one as
stale, implausible or excluded knows more. Without that, the overlay
would draw an excluded contributor as participating, when "the shed
sensor is why this went stale" is what it is opened to find.

- **Two health events, on transition**, `source-dropped` and
  `source-restored`, carrying `entity`, `aspect` and `source` (its
  `[sources]` name), recorded and read back like any event.
- **The SDK remembers, so the producer cannot forget**: the producer
  calls `ctx.source_used(entity, aspect, source, used)` every time it
  decides and it emits only on a change, the discipline hand-written
  producers get wrong.
- **A source with nothing on record is participating**, as declared.
  The first call per source always reports, so a consumer starting
  mid-window does not read silence as agreement. That report is
  best-effort: health events are not mirrored, so one published before
  the recorder subscribes is lost. If that misleads a house, the remedy
  is to mirror participation or make it queryable on the producer.
- **The overlay reads events from a week before its window**, so a
  source excluded earlier is not shown live for the whole span. It
  marks exclusion in the legend and keeps drawing the line, because
  the line stopping is the diagnosis.

Not ordinary state: a list is not one scalar, a boolean aspect per
source puts the source's name in the aspect's, and a count cannot
answer "which".

## Health, availability and the audit trail

Three different failures need three different signals. A unit that
dies is the supervisor's to report ([Supervision](#supervision)). A
device that dies behind a live unit is that unit's to report, as
state. Something a unit refused, dropped or noticed is a health event.
All three land on the bus, the recorder keeps the last two, and logs
stay outside the trail entirely.

### Health events

A unit has two voices about itself: its liveliness token, which the
supervisor turns into the status at `home/health/{unit}`, and health
events at `home/health/{unit}/event`
([Bus payload conventions](#bus-payload-conventions)). The status stays
the supervisor's: degradation is read from status and events, never
asserted by the unit. The recorder keeps every event, queryable by key
and window ([Read path](#read-path)).

- **The `drop` policy.** Input a unit cannot use never crashes it and
  always leaves one `drop` event with a `reason` and enough context to
  find the source (key, topic, device): `malformed-payload`,
  `invalid-command`, `unknown-device`, `non-scalar`, and an adapter's
  own. A silent drop is a bug, because a crash loop on poison input and
  data vanishing without a word are both worse than a line in the trail.
- **On transition, not per occurrence.** A persisting condition is
  reported when it starts and, where it has one, when it ends
  (`backend-outage`/`backend-restored`, one `device-silent` per down
  transition). A stream repeating every tick fills the events table and
  cannot be folded into intervals; a silent success stays silent.
- **Kinds are per producer, not a closed vocabulary.** Each unit
  documents its own; the SDK's (`restore-failed`, `source-dropped`,
  `source-restored`) and the recorder's are shared by every house.
- **Health events are not mirrored.** They are a stream of what
  happened, so one published before the recorder subscribes is gone.
  Anything a late joiner must see as current belongs in state.

### Availability

A device dropping out behind a live adapter is not unit liveness. The
mirror serves a bare value forever, and **publishing on transition
makes silence ambiguous**: the bus cannot distinguish "no change" from
"no sensor". Only the party with protocol knowledge can (a bridge's
availability report, a TCP session, a firmware's known cadence), so it
lives in the adapter.

- **Availability is ordinary state.** `available` (bool) is a base
  aspect orthogonal to capability, published on transition by the
  entity's owner. Recorded history, the mirror, a family-visible
  deviation on `false` and automations subscribing to it follow
  unbuilt.
- **Opt-in.** An adapter with a real loss signal publishes it; one with
  nothing to say does not fake one. A receive-timer signal makes the
  timeout a parameter, since the cadence is house knowledge, and
  reports the down transition as a health event.
- **Stale, not false.** On device loss the values stand and `available`
  flips; the adapter never publishes invented values, nulls or cleared
  keys. One boolean beside the values beats a tri-state smeared across
  every aspect.
- **`available` is reserved**: an adapter whose passthrough could mint
  it from a native field drops the field (`reserved-aspect`).
- **Commands toward an unavailable entity are per adapter** (refused
  as `device-unavailable`, or left for the device to miss).
- **No information is a state.** An entity with no `available` key is
  unknown, not up. Watching another entity's availability is a
  declared `[bus.subscribes]` binding, seeded from the mirror like any,
  never implicit, because that is the surface a manifest exists to
  show; an SDK helper answering up, down or unknown is not built.

Not a TTL on the mirror: the core cannot know a producer's cadence, and
a lock is rightly silent for months. Not timestamps: age without
cadence answers "when", not "should I trust this", and a value
published on transition is supposed to be old.

**The honest limitation.** `available` is device liveness, not data
freshness: a bridge's passive check on a battery device can take
hours, so a motion sensor dying mid-`occupancy = true` stays trusted
until then. Bounded-age input still needs the consumer's own policy.

### Staleness

Whether an input is too old to use, and what to do then, is the
consumer's policy, because it is house behaviour. The core enforces no
age anywhere, and no adapter invents one on a consumer's behalf.

- **`Freshness`** (`homeostat.freshness`) keeps the books per source:
  `seen(source, value, age_s)` records on the monotonic clock,
  `fresh(max_age_s)` returns what is within the automation's window at
  recompute time, `forget()` drops a source (on `available = false`).
- **A catch-up carries the mirror's age**, so a six-hour-old reading is
  not averaged in as new after a restart. A live trigger is age zero,
  so only a handler reachable from a catch-up must handle an empty set.
- **No timer lives in the helper**: reacting to silence is a
  `home/clock/minute` subscription calling the same `fresh()`.
- **The same rule holds for every class**: a forecast's consumer
  checks `issued` ([Forecasts](#forecasts)), a fed input expires in the
  device ([Device feeds](#device-feeds)), and a fusion goes stale
  ([Virtual sensors](#virtual-sensors)).

### One-way senders

A sub-GHz PIR, door contact or smoke detector transmits when something
happens and never sends a "clear". **The adapter owns the hold** that
decides when the assertion stops being true: the missing off is a
protocol fact, and an adapter may derive on its own bound entities. A
core-decayed momentary aspect would be the TTL
[Availability](#availability) refuses under another name.

- **Transitions only.** `true` on the first assertion, `false` when the
  hold expires; a repeat burst inside the hold extends the deadline
  silently, since these senders repeat every burst by design.
- **`false` for every bound entity at startup**, because the held value
  is the adapter's own construct, and so a crash-looping adapter cannot
  leave a sensor stuck on. Only a breaker-open adapter leaves `true`
  standing, visible in its health. The opposite of a latch
  ([Restoring a unit's own last value](#restoring-a-units-own-last-value)).
- **The hold is a parameter per aspect, not per entity**, chosen by
  capability and features with a generic fallback: a contact, a PIR and
  a detector want different holds, every contact the same one.
- **Availability is the bridge's, not a receive timer**: silence is a
  one-way sender's normal state.
- **A second one-way adapter moves the timer into the SDK**, as
  `Freshness` and `Cooldown` moved; never into the core.

Not consumer-side debouncing: every consumer would reimplement it,
differently, and the recorder could not reconstruct what was true when.

### Logs and the audit trail

**Logs are exhaust; events are the trail.**

- **Unit output is captured**: tagged on the supervisor's streams and
  kept in a 500-line ring per unit at `home/meta/{unit}/log` (MCP's
  `read_logs`, the dashboard's unit detail), gone on supervisor
  restart, never recorded ([The unit contract](#the-unit-contract)).
  Peripheral logs ride their
  adapter's stdout, tagged with the device, at warning and above so a
  chatty device cannot drown the ring.
- **The durable trail is the recorder's `events` table**: every health
  event, accepted parameter write and command envelope with its actor
  and band, read through `home/history/events`, MCP's `read_events`
  ([Agent surface (MCP)](#agent-surface-mcp)) and the dashboard.

Not a log sink in homeostat. Tagged stdout is the standard export
surface; durability, retention and indexing are deployment
configuration (Docker logging drivers, journald, Loki), which anything
built here would reimplement worse. And if a line matters enough to
query next week, that pressure must push the unit to emit a health
event, which a queryable log store, or stdout on the bus, would dissolve.

## Surfaces

A surface is where a person or an agent meets the house. Every surface
is a unit on the bus like any other, so it is supervised, has health,
and holds only the authority its manifest declares. None has a path
around [plan and apply](#plan-and-apply): the dashboard commands at the
manual band and edits family parameters, the agent surface only reads,
and every structural change goes through the house repo.

### Dashboard

The dashboard is an adapter for humans: HTTP and a WebSocket toward
browsers on one side, the bus through the SDK on the other. Browsers
never speak Zenoh. `adapters/dashboard.py` is a `service` unit
(aiohttp) serving one hand-editable page, `dashboard.html`, its
decision logic `assets/dashboard-logic.js` and a few vendored libraries
from an allowlist of filenames, with no build step. The page is
client-rendered because live state push is the dashboard's whole job.

- **Mediated, not raw bus.** Not Zenoh's remote-api plugin in the
  browser: it would bypass the grant table, the arbiter and the
  manifest-declared surface, and couple every client to the bus
  protocol. Through a unit the backend is existing plumbing: commands
  are manual-band envelopes, parameter edits take the
  [live parameter](#live-parameters) path, an opening page is a late
  joiner on [the last-value mirror](#the-last-value-mirror), and charts
  query the recorder.
- **Generated from the house's text.** The page is a pure function of
  the manifests, entity files, `zones.toml`, `dashboard.toml`, the grant
  table and the bus. Layout state exists nowhere else and there is no
  browser-side customisation, because hidden UI state is what the
  project exists to reject. Labels come from `[naming]` (`en` today).
- **The dashboard owns rendering; adapters never do.** Adapters speak
  homeostat vocabulary (capability, features,
  [aspect descriptors](#aspect-descriptors), constraints) and the
  mapping to controls lives in the dashboard alone. Not per-adapter UI:
  widgets would drift and the page would stop being a function of the
  house's text. A device class needing a new control extends the public
  vocabulary plus one rendering, which every adapter then gets.

The unit's model is the whole house, so it declares `inputs = "house"`
([Change detection](#change-detection)). It also re-parses the house
on `/api/model` at most every two seconds, keeping the last good model
when a half-written file fails to parse.

The whole HTTP API sits behind the gates in
[Local-only access](#local-only-access). `GET /api/model` is the house
rendered for the browser (each entity marked `commandable`, each unit
with what it drives and reads, the views, the `driven` bands);
`GET /ws` sends a snapshot of state, forecasts, holds, health, config
and descriptors, then live deltas; the writes are `POST /api/cmd`,
`/api/lights/off` and `/api/param`; the rest are read proxies onto the
recorder, the log tail and the camera relay, plus the map extract.
Live state reaches browsers through one bounded outbox per client (256
messages). A browser that stops reading is closed rather than buffered
for, and on reconnect takes a fresh snapshot, which is exactly what it
missed. The unit subscribes before it seeds from the mirror, so a live
update always supersedes the seed.

#### Commanding

Every command leaves at `priority = "manual"`, so "the family always
wins" falls out of the [arbiter](#arbitrated-mode), and manual-band
writers never count toward exclusivity ([Write modes](#write-modes)).
The manifest declares a blanket `home/cmd/**` publish per capability
the dashboard may command.

- **The dashboard honours its own grant table.** Nothing on the bus
  re-checks grants, so a blanket publish granted for `light` could
  carry a `climate` setpoint. The dashboard derives the capabilities it
  may command from its own cmd-class publishes, refuses `/api/cmd` for
  any other, and marks each entity `commandable` so ungranted controls
  render inert. This is the unit keeping its declaration, not a
  boundary ([Local-only access](#local-only-access)).
- **What it may command**: a capability's base aspect or a declared
  feature (type-checked, so a JSON object never rides an envelope), or
  an aspect whose descriptor declares a family-editable command
  (checked against its constraint). Bounds otherwise stay the adapter's.
- **Group actions fan out at the manual edge.** `POST /api/lights/off`
  sends one manual-band off per commandable light, lit or not. Not a
  commandable "scene" entity: its owner would re-publish at the
  automation band ([Priority bands](#priority-bands)) and be a second
  automation-band writer on every exclusive light.

#### The stages of a command

A command is a proposal, not a write ([Cmd envelopes](#cmd-envelopes)),
so the page never paints the request as the device's state: an
out-of-range setpoint returns `ok` and is then dropped by the adapter.
The control shows the request as one, with a line naming the stage
("asked 22.5° · still 21.0°") and then the outcome.

- **Taps build on the request**, not on a readback that has not moved,
  and settle for 600 ms before one command goes out. Each request
  replaces the previous one for that aspect.
- **Unheard is known at once.** `/api/cmd` replies with the envelope's
  `id` and `heard`: the owning unit holds its liveliness token and
  something subscribes where it listens (the arbiter's forward key for
  an arbitrated entity, the command key otherwise). Liveliness comes
  first because the recorder subscribes to every command.
- **Outcomes.** The asked value coming back **confirms**. A value held
  before the first tap, or asked on the way, is progress, because a
  polling bridge republishes the old value until the device moves. Any
  other value is **adjusted** (clamped or rounded), shown with both
  numbers. An arbiter `refuse` carrying the id is **held**, worded as a
  lost contest since a retry would lose identically. An adapter drop
  carrying the id shows its reason. Nothing within the wait is **no
  confirmation from the device**.
- **Matching is by `id`**, never by key and value, which cross wires
  exactly when someone taps twice. Values compare at the control's
  grain, so a bulb rounding by one step of 0–254 is not adjusted.
- **The wait** is the descriptor's `readback_s` (per command, then per
  entity), otherwise a generous per-capability guess. A timeout that
  fires early reports a failure that did not happen.

Not built: a "delivered to the device" event (every adapter would have
to emit it), and readbacks correlated to commands (each adapter would
decide which readback answers which command).

#### Charts, forecasts and sources

Charts query the recorder through `/api/history`, asking for `bucket`
sized to one point per drawn column for a number and `changes` for a
boolean or enum ([Read path](#read-path)). The descriptor decides
which, so an enum coded as integers is not averaged. `class=cmd` draws
a "Commanded" strip under the chart, the page's one view of intent
against outcome.

Forecasts reach the page live with state rather than from the recorder:
what the family sees is what the house believes now, and a house with
no recorder still sees its horizon. The current belief is drawn dashed
past the now line and captioned with when it was issued. Staleness is
the consumer's ([Forecasts](#forecasts)), and the dashboard's policy is
self-scaling: a belief older than the span it has left to say is drawn
grey and marked `stale`; one whose horizon has run out is no longer
drawn as the future. In the detail overlay, `forecasts` draws the
stored issues (`/api/forecasts`, the newest 40) and `sources` draws
each declared contributor ([Sources](#sources)) with its exclusions
from `/api/source-events`. Each is its own neutral line while the
outcome keeps the accent, never an envelope, whose edge traces a path
nobody predicted. Both are owner work, so they live in the overlay
rather than as widgets.

#### Controls and the overlay

The page maps descriptor vocabulary to controls and adds no
vocabulary. A command the family may not edit reads as a value.

- **Grain.** A derived grain is a twentieth of the range, rounded to
  something a person would say. Where the house knows better,
  `dashboard.toml` declares a `[[control]]` step, keyed by what is
  controlled, never by the widget placing it, so one grain holds
  everywhere. It is not a layout hint: a step says what a control does,
  not where it sits.
- **Accidental input.** A family parameter's caption prints its
  manifest default (`0–60 · default 5`), and range inputs take
  `touch-action: pan-y` so a scroll starting on a thumb does not
  command a device.
- **Parameters.** Every parameter is visible, and an owner-level one
  off its manifest default counts as a deviation, so a house running
  off its manifest is distinguishable from one running it. Only
  `editable_by = "family"` parameters get a control.

Tapping an entity, a reading or a unit opens a detail overlay, whose
width is a per-viewer `localStorage` preference. Two rules for anyone
changing it: **a reader's choices live outside the markup**, because
live state re-renders the panel and a pinned issue or highlighted
source held only in the DOM silently drops; and **nothing round lives
inside a chart SVG**, which stretches with
`preserveAspectRatio="none"`, so value dots are positioned in the
wrapper by percentage.

#### The page

`dashboard.html` and `dashboard-logic.js` are one artifact, served
`Cache-Control: no-cache`, because heuristic freshness would pair a new
page with old cached logic after an upgrade: a page that renders empty
over a healthy backend. Not versioned asset URLs: the version would
have to be rewritten into a hand-edited file.

Pure decisions (arithmetic and selection, never markup) live in
`dashboard-logic.js`, pinned by `node --test tests/js`.
`tests/browser` drives the real page against canned fixtures, whose
field names a canary in `tests/dashboard.rs` checks against a real
`/api/model`; it is a broad net, not a substitute for opening a
browser on a change.

### Views are text

`dashboard.toml` at the house root lists the views. Each `[[view]]` is
a nav entry holding either an ordered list of widgets from a closed
vocabulary or a generated view (`kind = "now" | "setpoints" |
"rooms"`), never both. [docs/widgets.md](widgets.md) shows each widget
and [docs/manifest.md](manifest.md) has the fields. The core validates
the file at `plan` like `zones.toml` and never renders it; it is a
house-wide input, so a view edit is a visible change that restarts the
dashboard.

- **The file replaces the nav; it does not augment it**, so a house can
  say "these three views are the dashboard". Without it the dashboard
  renders `Now`, `Setpoints` (every family parameter) and `Rooms`.
- **Nothing becomes unreachable.** Two things are fixed chrome, never
  in the file: **Health** (unit status and breakers, family-visible by
  design) and **Not shown**, every entity no widget places, drawn as
  usable room cards.
- **A view shows its text.** A read-only **Text** button renders the
  `[[view]]` block behind a view, a name to say to an agent working in
  the house repo. The dashboard never writes the house.
- **Placement is `dashboard.toml`'s alone.** An entity file carries no
  `[dashboard]` table: one way to place a thing.
- **`group` composes, one level deep.** Nested groups and per-widget
  layout hints (`span`, column counts) are refused: either would make
  the file a layout language, and layout is the dashboard's.

**`Now` shows the error signal, not an inventory**: people, the
deviations feed and the map. A house in equilibrium renders a nearly
empty page, deliberately. The `deviations` feed draws from:

- supervision: any unit not running;
- notable state from the capability vocabulary (lights on, as one row
  with the "All off" action), any entity with `available = false`
  ([Availability](#availability)), and any described aspect marked
  `notable` that reads true: vocabulary, never house configuration;
- parameters whose live value differs from the manifest default;
- arbiter holds that displaced somebody.

**A hold is a deviation when it displaced somebody.** The arbiter
leases every forwarded command, so most holds are the house working. A
hold is listed when it stands at a band above the lowest band anything
is granted to command that aspect at (`driven` in `/api/model`). Not
"once it has refused something": that depends on how often the
displaced automation publishes, a fact about its author. Not "every
hold": a family locking a door nothing automates has displaced nobody.
Possession still shows on the control as **held**.

**The unit card is a pure function of the manifest and the grant
table**: family setpoints, published entities, and what the unit
drives (its cmd grants) and reads (its expanded state subscriptions),
as `{entity, aspect}` rows because a relation is per aspect. If the
card is wrong, the manifest is.

### Map and people

A person is an entity with `capability = "person"` in the pseudo-room
`person`, because people move and the key space is room-keyed; which
room a person is in is state, never structure. Location is scalar
aspects ([the capability vocabulary](#the-capability-vocabulary)), so
position history is free. The `people` widget reads `presence` and
falls back to the age of the last fix.

The `map` widget is a view over every entity with a location. Tiles
are a self-hosted PMTiles extract named by `HOMEOSTAT_DASHBOARD_TILES`
and served by the dashboard unit, because a public tile CDN would learn
family positions from tile coordinates.

### Cameras

Pixels are the media plane; detections are data. The payload
conventions, the recorder and the mirror all assume small scalar JSON,
and once video bytes enter a homeostat process as data the small core
is gone.

- **Event plane: on the bus.** A camera is an entity
  (`capability = "camera"`) publishing scalar aspects, today `motion`,
  recorded and automatable exactly like a PIR's `occupancy`. A better
  detector later changes one adapter and no automation.
- **Media plane: off the bus, never recorded.** Live viewing rides RTSP
  into go2rtc, one upstream session per camera whatever the viewer
  count, restreamed as a pure remux. Stream names equal entity ids.
- **Browsers never speak go2rtc.** Its API is unauthenticated and can
  add streams, read back RTSP URLs with credentials and run `exec:`
  sources. So go2rtc binds to `127.0.0.1` with its other listeners off,
  and the dashboard relays `/api/camera/{entity}/live` byte for byte to
  go2rtc's `api/ws` behind its own gates, forwarding only the player's
  MSE request. Relaying is not processing: the bus, the recorder and
  the core stay scalar.
- **MSE, not WebRTC**, whose direct peer connection cannot ride the
  relay; MSE's 0.5–1.5 s latency is fine for a glance. **No
  snapshots**: a still from H.264 needs a ~100 MB transcoder the image
  does not carry. The stream starts only on tap.

**A foreign binary as a unit: the shim owns the token.** A Go binary
cannot declare a liveliness token, so `adapters/go2rtc.py` renders its
config from `HOMEOSTAT_CAMERAS` into a `0600` file outside the repo,
spawns the binary, polls its API until it answers, and only then
declares ready. Child death is shim exit is supervisor backoff. This
shim is the general answer for any foreign binary.

**Refused:** an NVR, motion detection, transcoding and frame storage
inside homeostat; mature tooling does each better, and a detector such
as Frigate is the growth path, as one more adapter.

### Notifications

Reaching a person is reaching a device the house binds. `notifier` is a
capability, an entity file per addressee binds it to a delivery
adapter, and an automation that wants to reach someone declares an
ordinary cmd-class publish onto that entity. The core adds nothing
beyond the vocabulary row.

- **Vocabulary.** `message` (base) and `alert` (feature) are
  commandable strings carrying the text itself. Severity is an aspect,
  not a payload field, so the two are separately grantable, separately
  policed by the adapter (quiet hours may withhold `message`, never
  `alert`) and separately recorded. `delivered` is the epoch time the
  delivery service acknowledged the last message, never a human's
  receipt. Chat ids, topics and priorities are the adapter's dialect.
- **Addressing is the entity**: pseudo-room `person` for one person's
  phone, `global` for a group channel. A person with two channels is
  two entities, and switching providers changes no automation.
- **Gating is the grant table, unchanged.** A new `notifier` publish is
  a grant delta, so the plan is [structural](#tiers) and shows who may
  reach whom. Channels are `shared`, so the band is inert; `actor` is
  what the recorder keeps with every message.
- **Rate limiting splits in two.** The cooldown is house policy, a
  family-editable parameter kept by the SDK's `Cooldown`; the adapter's
  own floor is defence in depth, dropping with `rate-limited`.
- **Failure is loud.** A delivery adapter verifies its server before
  `ready()`. An undelivered message is a `drop` with `delivery-failed`
  and the channel's `available` goes false until the next success, a
  deviation on `Now`.

**Not used:** an external subscriber (it cannot carry intent: "skipped
because it rained" is not derivable from state); health events (no
addressee); a `home/notify/**` class with a routing service (the
addressee becomes a name the plan cannot check); an SDK facility
(authority by import); dashboard web push (no secure context, and
looking at the dashboard is not being told).

### Agent surface (MCP)

`homeostat mcp` is an MCP server through which an agent observes the
house. It is read-only and a pure bus client: it takes no house root,
never reads the repo, and never shells out to git. An agent changes the
house the way everyone does, by editing the house repo and running
`homeostat plan`, and the owner applies.

| Tool | Reads |
|---|---|
| `read_state` | any `home/**` key expression through the core's last-value caches |
| `read_history` | `home/history/{state\|cmd}/{entity}/{aspect}` with `from`/`to`, `limit`, and the folds `bucket` or `changes` ([Read path](#read-path)) |
| `read_logs` | a unit's captured output ring buffer |
| `read_events` | the audit trail at `home/history/events` |
| `schema` | the manifest contract as JSON Schema |
| `explain` | the registered paragraph for a validation error code |

`schema` and `explain` let an agent writing manifests read the rules
the validator enforces rather than its source
([The manifest is the contract](#the-manifest-is-the-contract)). The
discovery loop is in [Discovery](#discovery).

- **Transports.** Stdio (`--bus <endpoint>`), launched by an MCP client
  for local work. HTTP (`--http <addr>`) for a deployed house, as a
  `service` unit the house opts into, so it is supervised like any
  unit. HTTP is stateless streamable-HTTP (a POST carries one JSON-RPC
  message and gets `application/json` back; GET is 405) behind the
  dashboard's gates ([Local-only access](#local-only-access)).
- **Hand-rolled protocol**: `initialize`, `tools/list`, `tools/call`
  and `ping`. An MCP SDK would be the largest dependency in the tree
  for four methods.

**No write tools.** Every agent in use works in a checkout of the house
repo, where the CLI already gives it plan/apply with git review. And a
write path committing into the supervised tree puts unapproved code
where units spawn from: a pending plan gates the restart, not the file,
so new code would run at the next crash. A write side would first need
proposals staged outside that tree and the tier ceiling enforced in
the supervisor's apply path.

### Voice

Voice is planned, not built. It is held to: a narrow, high-precision
fast-path intent matcher with the conversational agent as fallback; a
fast-path grammar generated house-side from manifests and the key
space at plan/apply time, so the public tool never sees private
naming; local wake word and speech-to-text, no cloud in the fast path;
short-lived, satellite-scoped agent sessions. A satellite is a
manual-band, family-tier surface like the dashboard, fanning group
commands out at the edge.

## Security model

Homeostat has no accounts, no login and no TLS. A stranger is kept out
because the house's surfaces are reachable only from its own network; a
family member cannot rewire the house because no surface they reach
has a structural path. Every new surface keeps both rules.

### Local-only access

**Reachability is the credential.** The house is reached on its LAN, or
over WireGuard for phones and remote devices. Anything that can reach a
surface is treated as the family ([Family tier only](#family-tier-only)),
so the gates below are structural rather than authentication.

**The bus port matters most.** A cmd envelope's `priority` and `actor`
are self-declared and checked only for shape
([Bus payload conventions](#bus-payload-conventions)), and nothing on
the bus re-checks grants. Anything that can publish on the Zenoh port
(7447) can command every entity, outbid the arbiter by claiming the
top band, and forge state. So the bus is never published to the
network:

- The starter's compose file publishes the dashboard and the MCP port
  and deliberately not 7447; `plan` and `apply` run through
  `docker compose exec`. `127.0.0.1:7447` is no boundary either: a
  container on `network_mode: host` shares the host's loopback.
- Grants describe what a unit declared, not what it can do: a unit
  opening its own session can publish anything. Making them constrain
  takes a bus credential per unit, not a check in each adapter.

**The browser is not local, even when the dashboard is.** A public page
open in a family member's browser can fire requests at LAN addresses
(CSRF), and by DNS rebinding can make the browser treat a house address
as the page's own origin and read the replies. So every HTTP surface
carries three gates:

| Gate | Dashboard | MCP over HTTP |
|---|---|---|
| `Host` is a house-network address or a listed name | every request | every request |
| `Origin`, when present, passes the same host rule | WebSocket handshakes | every request |
| `X-Homeostat` header present | every `POST` | every request |

- **`Host`** defeats DNS rebinding: a rebound public domain arrives
  under its own name and is refused. A house-network address is a
  private, loopback, link-local or unspecified one (IPv6 unique-local
  included), where a LAN or a WireGuard tunnel lands. The listed names
  (`localhost`, `homeostat`, `homeostat.lan`, `homeostat.local`) extend
  through `HOMEOSTAT_DASHBOARD_HOSTS` and `HOMEOSTAT_MCP_HOSTS`, not the
  repo. Both implementations are pinned against one table in
  `tests/fixtures/host_gate.json`.
- **`Origin`**, sent on a WebSocket handshake and a cross-origin
  request, fails the host rule for a foreign page or `null`.
- **`X-Homeostat`** is a header a cross-origin `fetch` cannot add
  without a CORS preflight, which nothing answers. Without it a
  cross-origin `text/plain` POST is a "simple request" that reaches the
  server unpreflighted and could drive a write blind. The MCP server
  requires it on reads too, because what it serves is the house's
  private record.

The MCP server refuses before reading a body, never echoes the reason
to a browser, and bounds what a LAN peer can make it hold; the
dashboard caps bodies at 64 KiB. A foreign service with an
unauthenticated API binds to `127.0.0.1` and is reached only through a
unit's relay, as go2rtc is ([Cameras](#cameras)).

**Plain HTTP, and no PWA.** Service workers need a secure context even
on private addresses, so the dashboard is plain `http` and a bookmark.
A private CA is a plausible later path; nothing architectural depends
on it.

**Secrets never enter the repo**, because a repo is copied, pushed,
reviewed and read by agents ([Repo split](#repo-split)). A unit sees
only a fixed base environment plus the variables its manifest names in
`[runtime] env` ([The unit contract](#the-unit-contract)), so a token
meant for one unit never reaches another. Per-device secrets live in a
TOML file outside the checkout named by an environment variable
(`HOMEOSTAT_CAMERAS`, `HOMEOSTAT_MQTT_CREDENTIALS`), and files a unit
renders from them are written `0600` outside the repo and deleted on
exit. A non-secret endpoint is ordinary repo content in `[discovery]`.

### Family tier only

Anyone who can reach the dashboard is `family`. There is no owner mode,
no admin panel and no approval surface on it, and it never grows one;
the owner acts through git and the CLI (`plan`, review, `apply`). What
the dashboard can do is exactly what the family tier may do:

- send manual-band commands within its own grants, checked against the
  vocabulary or the adapter's declared constraint
  ([Commanding](#commanding));
- write `editable_by = "family"` parameters within their constraints
  ([Live parameters](#live-parameters));
- read state, history, health and logs.

Nothing structural (a grant, a manifest, an entity binding, a unit's
code) is reachable from it: a stolen phone inside the perimeter can
nudge setpoints and switch lights, not rewire the house. The agent
surface holds the same line by being read-only
([Agent surface (MCP)](#agent-surface-mcp)), and voice will be
family-tier on the same terms.

## Distribution

### Repo split

- **Public (`homeostat`, this repo):** the Rust core, the manifest
  schema ([docs/manifest.md](manifest.md), versioned by each file's
  `schema` field), the Python SDK, the generic units in `adapters/`,
  and two example houses: `examples/house`, documented and the plan
  test corpus, and `examples/starter-house`, the template a new house
  starts from.
- **Private (the house repo):** every manifest, entity file and zone,
  the automations, `dashboard.toml`, pending plans, and house-specific
  agent instructions. It pins a release (image tag and SDK version),
  and its CI can run `homeostat plan`, which without a bus plans
  offline and exits non-zero on any validation error.
- **Boundary test:** a device address, a family member's name, a room
  name or a behavioral choice is private; anything identical in a
  stranger's house is public, and generic automations graduate into
  SDK helpers or adapters. The public tool never sees a private repo
  except locally.

### Release artifacts

A release (tag `vX.Y.Z`) publishes, each with a signed build provenance
attestation: `homeostat` tarballs for `x86_64` and `aarch64` Linux,
the SDK wheel, and the image `ghcr.io/freol35241/homeostat` (tags
`X.Y.Z` and `X.Y`, `linux/amd64` and `linux/arm64`), with `SHA256SUMS`
for the tarballs and wheel.

The binary reports its version and commit at `home/meta/system/about`.
The version lives in `Cargo.toml`, `sdk/python/pyproject.toml`, the
starter's compose file and `scripts/sync_starter.sh`, whose check fails
when they disagree: a wrong version reported is worse than none.

### The container image

The image (`Dockerfile`) is the deployment boundary: one container
holds the core and every unit as plain processes
([Process model](#process-model)). It carries the `homeostat` binary;
`git`, for `plan --save` and apply's commit provenance; `tini` as
PID 1; `tzdata`; uv with a pre-installed CPython 3.12, so first boot
downloads no interpreter; the SDK wheel in `/opt/homeostat-wheels`,
with `UV_FIND_LINKS` pointing there; and the `go2rtc` binary the camera
unit spawns, checksum-pinned per architecture. Provisioning a binary is
the image's job, never the house repo's.

It runs as an unprivileged user (uid 1000, or any via `--user`), with
the house repo at `/house` and `/var/cache/uv` worth a volume so unit
environments survive container replacement. The default command is
`up /house --listen tcp/0.0.0.0:7447`, and `HOMEOSTAT_BUS` is preset to
loopback, so `docker exec <container> homeostat apply /house` needs no
address. Without Docker, a house runs from the release binary and the
wheel, with `UV_FIND_LINKS` pointing at the wheel's directory.

Port 7447 is exposed for sibling containers but must not be published
to the host: reaching the bus is full authority over the house
([Local-only access](#local-only-access)). Host networking is not
required, since the bus uses explicit endpoints; an adapter that
discovers by multicast (mDNS) sees only what reaches the container's
network and falls back to explicit addresses.

### SDK distribution

A house unit names the SDK by exact version in its PEP 723 block,
`"homeostat==X.Y.Z"`, with no `[tool.uv.sources]`, and uv resolves it
from the bundled wheel through `UV_FIND_LINKS`.

- **The pin is in the unit script, so `files_hash` covers it.** An SDK
  bump is a visible behavioral change in `plan`, restarted by `apply`.
  Not a vendored SDK copy, nor a floating or path dependency: both sit
  outside change detection.
- **No clone and no network** at first boot. Not a git source: it
  needs both, and "pinned to a tag" invites the same-commit trap below.
- **The cost:** a house pinning a version the image does not bundle
  fails to resolve at unit start, the version-floor hazard in its
  loudest form: the unit never reaches `running` and its log says why.

Inside this repo, `adapters/` and test fixtures use an editable `path`
source so tests exercise the working-tree SDK, and each adapter script
has a uv lockfile beside it (`{script}.py.lock`), so a unit resolves
the dependency versions its release was tested against.

**Adapter and SDK must come from the same commit.** An adapter from
`main` fails against an older SDK with `AttributeError`, so a house
copies an adapter from the release it pins.

### The starter house

`examples/starter-house` is a self-contained house repo: copy it out,
`git init`, and run its compose file (mosquitto, Zigbee2MQTT and the
image). Its copies of the generic adapters, their lockfiles and the
dashboard's assets are generated, never edited: `scripts/sync_starter.sh`
takes each from `adapters/` at the release tag the starter pins
(`SDK_TAG`) and rewrites the SDK source to the `homeostat==X.Y.Z` pin
and the lockfile's SDK entry to the bundled wheel. So the starter is a
snapshot of a release, not of `main`, and CI's `sync_starter.sh
--check` fails when any copy differs from what that release generates.
A release bumps `SDK_TAG` with the other version strings; until the
tag exists the check compares against the working tree, the release
commit itself. CI checks out full history, or the tag is never found.

## Open questions

Questions the design has named and not answered, and known gaps it
has not closed.

- **Should `features` gate command contents?** The grant table does not
  check a command's value or aspect against them; the SDK and the
  adapter do. The lean is no separate layer. See
  [The grant table](#the-grant-table).
- **Access control on the bus.** Reaching the bus is full authority;
  Zenoh ACLs would make grants a runtime boundary, and when to take
  that on is open. See [Local-only access](#local-only-access).
- **`editable_by` is enforced by the dashboard only.** The core's
  config queryable checks type and constraint, so any bus client may
  write an owner-tier parameter. See [Live parameters](#live-parameters).
- **Turning a live edit into a commit.** A family member's live edit
  is reverted by the next apply unless someone commits it; capturing it
  automatically is unbuilt. See [Live parameters](#live-parameters).
- **Handing an arbiter hold back early.** Only expiry ends a hold;
  whether release belongs on the envelope or on an arbiter surface is
  open. See [Arbitrated mode](#arbitrated-mode).
- **A cmd publish with no `priority`** is treated as `automation` by
  `plan` but refused by the SDK. See [Priority bands](#priority-bands).
- **Lockfiles are outside change detection.** `files_hash` does not
  cover a script's `{script}.py.lock`, so a lock-only change neither
  shows in `plan` nor restarts the unit. See
  [Change detection](#change-detection).
- **Forecast uncertainty and corrections to the past** have no
  representation: a point is one scalar, and `state` keeps no
  reanalysis. See [Forecasts](#forecasts).
- **An inhibit class for interlocks**, a condition-held lockout the
  arbiter honours against every band, is deferred until a second case
  needs it. See [Burners and interlocks](#burners-and-interlocks).
