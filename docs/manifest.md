# Manifest reference

Generated from the manifest structs by `homeostat schema --markdown`; do not edit (a test refuses a stale copy). The same schema is served as JSON by `homeostat schema [unit|entity|zones|dashboard]` and the MCP `schema` tool. Rules the validator enforces beyond the shape are named by their error code; `homeostat explain <code>` (or the MCP `explain` tool) has the paragraph for each. Reasoning lives in docs/design.md.

## Unit manifest (`units/<name>.toml`)

A unit manifest: `units/<name>.toml`. One schema, three kinds (adapter,
automation, service); which sections a kind accepts is stated on each
section, and the validator refuses a mismatch with `invalid-manifest`.

| Field | Type | Required | Description |
|---|---|---|---|
| `bus` | [BusSection](#bussection) | no | The unit's declared bus surface, and its only one. The SDK refuses a publish outside it. |
| `discovery` | [DiscoverySection](#discoverysection) | no | How an adapter reaches its backend. Required for adapters, allowed for services, refused for automations. |
| `entities` | [EntitiesSection](#entitiessection) | no | The entities this unit binds. Required for adapters, allowed for automations (virtual sensors), refused for services. |
| `naming` | [UnitNaming](#unitnaming) | no | Human names for voice and the dashboard. The dashboard reads well only when these are set. |
| `params` | table of name → [ParamSpec](#paramspec) | no | Live parameters, keyed by name (`home/config/{unit}/{param}`). Names become key segments (`invalid-name`). |
| `runtime` | [RuntimeSection](#runtimesection) | yes |  |
| `schema` | integer | yes | Contract version. Must be 1 (`unsupported-schema` otherwise). |
| `unit` | [UnitSection](#unitsection) | yes |  |

### BusSection

`[bus]`: the unit's declared key surface. Keys are `home/{class}/...`
(`key-outside-schema` otherwise). A zone name in the room slot expands
to its rooms at plan time. `{room}`/`{entity}` templates expand per
bound entity. They are valid only in adapters and automations
(`template-outside-binding-unit`), and only in a unit that binds
entities (`template-without-entities`).

| Field | Type | Required | Description |
|---|---|---|---|
| `publishes` | table of name → [PublishSpec](#publishspec) | no | `[bus.publishes]`: binding name → what the unit may publish. This is what the plan resolves into the grant table. |
| `subscribes` | table of name → string | no | `[bus.subscribes]`: binding name → key expression. The SDK subscribes by binding name. A unit's own `home/config/{unit}/*` is implicit and is not declared. |

### DiscoverySection

`[discovery]`: how an adapter finds its backend. `static` needs
`endpoint`, `mdns` needs `service` (`invalid-manifest` otherwise).

| Field | Type | Required | Description |
|---|---|---|---|
| `endpoint` | string | no | A URL such as `mqtt://host:1883`. `${VAR}` expands from the unit's environment at start; an unset variable is a startup error. |
| `mode` | [DiscoveryMode](#discoverymode) | yes |  |
| `service` | string | no | The mDNS service type to browse, e.g. `_esphomelib._tcp`. |

### EntitiesSection

`[entities]`: where this unit's entity files live.

| Field | Type | Required | Description |
|---|---|---|---|
| `dir` | string | yes | Directory relative to the house root, e.g. `entities/zigbee/`. Each `<name>.toml` in it is one bound entity; the stem is its name (`missing-entities-dir` if absent). |

### UnitNaming

`[naming]` on a unit.

| Field | Type | Required | Description |
|---|---|---|---|
| `aliases` | list of string | no | Alternative spoken names. |
| `en` | string | no | English label; the dashboard falls back to the name with `_` replaced by spaces. |
| `room` | string | no | Zone or room the unit belongs to, for grouping. |
| `sv` | string | no | Swedish label. |

### ParamSpec

`[params.<name>]`: one live parameter. The manifest default is the
value until something writes `home/config/{unit}/{param}`; a write
outside the constraint is refused with the old value still in force.

| Field | Type | Required | Description |
|---|---|---|---|
| `constraint` | table | no | Inline table of constraint keys the type understands (`malformed-constraint` otherwise): `min`/`max` for `int` and `float`; `after`/`before` (`"HH:MM"`, may span midnight) for `time`; `enum` (a non-empty list of strings) for `string`. |
| `default` | any | yes | A TOML literal of the declared type (`invalid-default` otherwise): `true`, `30`, `0.5` (an integer literal is accepted for `float`), `"text"`, or `"22:00"` for `time`. Must satisfy the constraint. |
| `editable_by` | [EditableBy](#editableby) | no | Who may change it live. `family` params appear as editable setpoints on the dashboard; anything else is visible there but written only through the repo or the bus. |
| `type` | [ParamType](#paramtype) | yes |  |

### RuntimeSection

`[runtime]`: how the supervisor runs the unit.

| Field | Type | Required | Description |
|---|---|---|---|
| `command` | string | yes | The command line, split on whitespace and run without a shell from the house root, with `HOMEOSTAT_UNIT` and `HOMEOSTAT_BUS` set. Typically `uv run units/<name>.py`. |
| `env` | list of string | no | Names of the environment variables this unit reads, passed through from the supervisor's environment by exact name (e.g. `["HOMEOSTAT_NTFY_TOKEN"]`). A unit sees nothing else of the supervisor's environment beyond a fixed base set (`PATH`, `HOME`, locale, `TZ`, `UV_*`, `PYTHON*`, CA bundles) and the `HOMEOSTAT_UNIT`/`HOMEOSTAT_BUS` it is given. A secret meant for one unit is therefore not visible to another. |
| `restart` | [RestartPolicy](#restartpolicy) | yes |  |
| `shutdown_grace_s` | integer | no | Seconds between SIGTERM and SIGKILL at shutdown. Default 5. |

### UnitSection

`[unit]`: identity.

| Field | Type | Required | Description |
|---|---|---|---|
| `description` | string | no |  |
| `kind` | [UnitKind](#unitkind) | yes |  |
| `name` | string | yes | Unique across the house; a bus key segment (`home/health/{unit}`, `home/config/{unit}/*`), so letters, digits, `_`, `-`, `.` only, and not `.` or `..`. `system` is reserved for the core. |
| `watches` | [UnitWatches](#unitwatches) | no | Which repo files restart this unit when they change. Absent means `own`: the unit's command, its own entity files, its zone if it uses one. `inputs` is accepted as an older spelling. |

### PublishSpec

One publish the unit is allowed.

| Field | Type | Required | Description |
|---|---|---|---|
| `capability` | string | no | Required under `home/cmd/` (`publish-missing-capability`): the grant resolves onto bound entities of this capability that the key covers. Must be a known capability (`unknown-capability`). |
| `key` | string | yes | Key expression. Under `home/state/` it must name a bound entity's room and entity literally or by template (`state-publish-unbound`). Under `home/forecast/` the entity must exist but need not be one this unit binds (`forecast-publish-unbound`). The key carries a sixth segment naming the forecast's source. Several sources can then forecast one series without overwriting each other (`forecast-publish-conflict`). |
| `priority` | [Priority](#priority) | no | The band commands leave at. Required under `home/cmd/` (`publish-missing-priority`). Automations publish at `automation`; the family's surfaces (dashboard, voice) at `manual`, which always wins in arbitration. |

### DiscoveryMode

How the backend address is obtained.

- `static` — One configured `endpoint`.
- `mdns` — Each device resolved individually by browsing `service`.

### EditableBy

Actor tiers that may write a parameter live.

- `owner`
- `family`

### ParamType

Parameter value types.

- `bool`
- `int`
- `float`
- `string`
- `time` — A wall-clock time of day, `"HH:MM"`; the SDK serves it as a `datetime.time`.

### RestartPolicy

When the supervisor restarts an exited unit. Every restart carries
backoff and a circuit breaker; the policy only says which exits count.

- `always` — Restart after any exit, clean or not.
- `on-failure` — Restart only after a non-zero exit or a signal.
- `never` — Never restart; a stopped unit stays stopped until the next apply.

### UnitKind

What a unit is, which decides the sections it may carry and how it is
ordered in an apply walk.

- `adapter` — Puts devices on the bus: binds entities, needs `[discovery]` and `[entities]`, publishes their state, takes their commands.
- `automation` — Regulates: subscribes state, publishes commands at the automation band, and may bind read-only virtual entities via `[entities]`.
- `service` — Infrastructure with no entities (the recorder, the dashboard, the arbiter, the clock). May carry `[discovery]`.

### UnitWatches

Which files restart a unit when they change. A unit whose model covers
the whole house, such as the dashboard, depends on every entity file and
manifest, not only the files it owns. Per-unit change detection cannot
infer that, so the unit declares it. Without the declaration, `apply`
reports success and leaves the unit running on stale files.

- `own` — The unit's own files: its command, its entity files, its zone.
- `house` — Every manifest and entity file in the house.

### Priority

Command priority bands, lowest to highest.

- `automation` — Automations. The lowest band.
- `agent` — Above `automation`.
- `family` — Above `agent`.
- `manual` — The family's own surfaces (dashboard, voice). The highest band, and exempt from exclusive-write checks.

## Entity file (`<entities dir>/<name>.toml`)

An entity file: `<entities dir>/<name>.toml`. The file stem is the
entity's name, unique across the house (`duplicate-entity-name`), and
a key segment (`invalid-name`).

| Field | Type | Required | Description |
|---|---|---|---|
| `entity` | [EntitySection](#entitysection) | yes |  |
| `inputs` | table of name → [InputSource](#inputsource) | no | `[inputs]`: device inputs fed from one source each, keyed by the adapter's own input name (e.g. `indoor_temperature_actual`). A fed input is a continuous signal with one source, not a command. It stops being a command aspect for this entity and does not go through the arbiter. The device's own validity window decides when it is stale. Only a device entity can be fed (`virtual-entity-fed`). The adapter decides which input names exist. |
| `naming` | [EntityNaming](#entitynaming) | no |  |
| `schema` | integer | yes | Contract version. Must be 1. |
| `sources` | table of name → [SourceRef](#sourceref) | no | `[sources]`: the readings this entity's value is derived from, keyed by a short name for each contributor. The history overlay draws them beside the computed value (docs/design.md#sources). They must be declared because they cannot be inferred: a unit subscribes to many keys for many reasons, and its subscriptions do not say which feed which published aspect. This is separate from `[inputs]`. A device feed carries a runtime contract that does not apply here. A wired input also stops being a command aspect, which would collide on a commandable virtual entity. |
| `write_policy` | [WritePolicy](#writepolicy) | yes |  |

### EntitySection

`[entity]`: what the device is and where.

| Field | Type | Required | Description |
|---|---|---|---|
| `capability` | string | yes | One of: `binary_sensor`, `burner`, `camera`, `climate`, `cover`, `light`, `lock`, `notifier`, `person`, `presence`, `router`, `sensor`, `switch` (`unknown-capability`). Decides the base aspect, the dashboard widget and which cmd grants apply. |
| `features` | list of string | no | Optional aspects beyond the capability's base, as the adapter names them (`brightness`, `color_temp` on a light). For a sensor it is descriptive only; its widgets come from the numeric aspects it publishes. |
| `id` | string | no | The adapter-native address (a zigbee2mqtt friendly name, an ESPHome node, a camera's go2rtc stream). Unique per adapter (`duplicate-entity-id`). Required on an adapter-owned entity, where it addresses something (`entity-id-required`); optional on an automation-owned one, which has no periphery to address and would otherwise have to invent a name for a device that does not exist. |
| `room` | string | yes | Where the entity is. No other file places it. A key segment; not `home` or a key class (`reserved-room-name`). The pseudo-rooms `global` and `person` are for entities with no place. |

### InputSource

The source of a fed input: an entity and one of its aspects, i.e. the
state key `home/state/{room}/{entity}/{aspect}`. The plan resolves it
and prints the edge, as it does a grant.

| Field | Type | Required | Description |
|---|---|---|---|
| `aspect` | string | yes | The aspect to read. When the source is automation-owned, that automation's `[bus.publishes]` must cover the key (`input-unpublished-aspect`). |
| `entity` | string | yes | Name of the source entity; must exist (`input-unknown-entity`). Any owner will do: an automation's virtual sensor or another adapter's device. |

### EntityNaming

`[naming]` on an entity.

| Field | Type | Required | Description |
|---|---|---|---|
| `aliases` | list of string | no | Alternative spoken names. |
| `en` | string | no | English label; the dashboard falls back to the name with `_` replaced by spaces. |
| `sv` | string | no | Swedish label. |

### SourceRef

One entry in `[sources]`: a reading that a computed value is derived
from, named the same way a device feed names its source, because the
identity layer between a unit and a bus key is the same one
(docs/design.md#device-feeds).

| Field | Type | Required | Description |
|---|---|---|---|
| `aspect` | string | yes | The aspect that contributes. When the contributor is automation-owned, that automation's `[bus.publishes]` must cover the key (`source-unpublished-aspect`). |
| `entity` | string | yes | Name of the contributing entity; must exist (`source-unknown-entity`). Any owner will do. |
| `note` | string | no | Free text about this contributor, shown beside it in the history overlay. Use it for what the subject's own descriptor cannot say because it is not true of every source. For example, one sensor sits in the sun, or a reading carries an offset the house itself writes and so must not be fused back in. Kind and unit stay on the aspect descriptor, which every source is held to. |
| `precision` | number | no | The contributor's resolution in the aspect's own unit, where it differs enough to matter. A half-degree sensor read against a hundredth-degree one looks like it disagrees when it is only coarse. |

### WritePolicy

`[write_policy]`: who may command the entity and how conflicts resolve.

| Field | Type | Required | Description |
|---|---|---|---|
| `mode` | [WriteMode](#writemode) | no | How commands are governed. Required on a capability that takes commands, which is one with a base aspect such as `light` or `lock` (`write-mode-required`). A commandable entity's policy is then always written in its own file. Optional elsewhere (`sensor`, `camera`, `router`, …), where a mode governs nothing. When absent it reads as `shared`. |
| `owner` | string | yes | The one unit that binds this entity: an adapter, or an automation for a virtual entity. Must exist (`missing-owner-unit`) and be the unit whose entities dir holds this file (`owner-mismatch`). |

### WriteMode

How commands toward the entity are governed. An automation-owned
(virtual) entity is a latch when commanded: `arbitrated` is refused
(`virtual-entity-arbitrated`), and a cmd grant may cover it only when
the owner subscribes to its cmd keys (`virtual-entity-commanded`).

- `shared` — Any granted writer may command it; last write wins.
- `exclusive` — At most one unit below the manual band may be granted (`exclusive-write-conflict`); manual-band surfaces sit above.
- `arbitrated` — Commands go through the arbiter (leases, bands, preemption); the house must bind an arbiter-class publish covering it (`arbitrated-uncovered`).

## Zones (`zones.toml`)

`zones.toml` at the house root: named sets of rooms that expand in the
room slot of key expressions.

| Field | Type | Required | Description |
|---|---|---|---|
| `schema` | integer | yes | Contract version. Must be 1. |
| `zones` | table of name → list of string | no | `[zones]`: zone name → member rooms. A zone name is a key segment, not reserved (`reserved-zone-name`), not also a room (`zone-room-collision`); members must be rooms some entity binds (`zone-unknown-room`) and not pseudo-rooms (`zone-pseudo-room`). |

## Dashboard views (`dashboard.toml`)

`dashboard.toml` at the house root: the family surface's views, each a
nav entry composed of widgets over things the house already has.
The file is optional. Without it the dashboard renders its generated
views (Now, Setpoints, Rooms). When present it is the whole nav. Health
and the list of everything not shown stay reachable as fixed chrome
outside the views. Layout lives as text in the repo and not as
browser-side state (docs/design.md#views-are-text).

| Field | Type | Required | Description |
|---|---|---|---|
| `control` | list of [ControlSpec](#controlspec) | no | `[[control]]`: the grain a control moves in, per thing controlled. |
| `schema` | integer | yes | Contract version. Must be 1. |
| `view` | list of [ViewSpec](#viewspec) | no | `[[view]]`: the nav, in order. |

### ControlSpec

One `[[control]]`: how coarse a slider is for the thing it controls.
It is keyed by what is controlled (an entity's aspect or a unit's
parameter) and not by the widget that places it. The same control is
drawn on a room card, in a view and in the detail overlay, and a step
that differed between them would look like a bug.

Without an entry the dashboard uses a twentieth of the range. The step
changes how the control behaves. It is not a layout hint.

| Field | Type | Required | Description |
|---|---|---|---|
| `aspect` | string | no | With `entity`: the commandable aspect. A key segment. |
| `entity` | string | no | With `aspect`: the entity whose aspect this control commands. |
| `param` | string | no | With `unit`: the parameter, by name. |
| `step` | number | yes | The step the slider moves in, and the nudge its ± buttons make. Positive and finite (`dashboard-control-step`). |
| `unit` | string | no | With `param`: the unit whose parameter this control edits. |

### ViewSpec

One `[[view]]`: either a generated view kept as is (`kind`) or a
composition of widgets (`widgets`), not both (`dashboard-view-shape`).

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | [GeneratedView](#generatedview) | no | A generated view, kept as the dashboard renders it without this file. |
| `label` | string | no | Nav label; the name, title-cased, when absent. |
| `name` | string | yes | Unique among views (`dashboard-duplicate-view`); a key segment (`invalid-name`); not `health` or `notshown`, the fixed chrome's own names (`dashboard-reserved-view`). |
| `widgets` | list of [WidgetSpec](#widgetspec) | no | The view's widgets, in order. |

### GeneratedView

The dashboard's generated views. Health is not among them: it is
fixed chrome beside "Not shown", reachable whatever the file says.

- `now` — People, the deviations feed and the map: the error signal.
- `setpoints` — Every family-editable parameter as one flat list.
- `rooms` — The room-card grid over every entity.

### WidgetSpec

One widget on a view. Which fields it takes is fixed per kind
(`dashboard-widget-fields`); references must resolve
(`dashboard-unknown-entity`, `dashboard-unknown-room`,
`dashboard-unknown-unit`). The dashboard does all rendering. A widget
says what to place, not how it looks.

| Field | Type | Required | Description |
|---|---|---|---|
| `aspect` | string | no | `chart`: the aspect charted; `tile`: narrows the tiles to one reading (every reading otherwise); `dial`: the temperature command to turn (the first one, or the climate setpoint, otherwise). A key segment (`dashboard-invalid-aspect`). |
| `entity` | string | no | `tile`, `chart`, `entity`, `dial`, `burner`: the entity, by name. `burner` takes one of `capability = "burner"` (`dashboard-widget-capability`). |
| `hours` | number | no | `chart`: the window in hours (24 when absent). |
| `kind` | [WidgetKind](#widgetkind) | yes |  |
| `label` | string | no | `group`: the label over its members; unlabelled when absent. |
| `room` | string | no | `room`: the room whose card to place. |
| `unit` | string | no | `unit`, `params`: the unit, by name. |
| `widgets` | list of [WidgetSpec](#widgetspec) | no | `group`: the widgets it draws as one card, in order. A group cannot hold another group (`dashboard-nested-group`). |

### WidgetKind

What a widget places. Everything is rendered by the dashboard from the
house's text, the grant table and the bus; no widget carries markup.

- `tile` — A signal tile per reading of an entity: the value big, today's range under it.
- `chart` — One aspect's history over a window.
- `entity` — An entity's own row (its control or its readings) as a card.
- `dial` — A thermostat dial: a temperature setpoint on an arc, the current reading beneath it.
- `room` — A room's card: every entity in the room.
- `unit` — A unit's card: its family setpoints, the entities it publishes, the entities it drives (from the grant table) and the entities it reads (from its subscriptions). All of it derives from its manifest.
- `params` — A unit's family-editable parameters as one card.
- `people` — The person entities, home or away.
- `deviations` — The deviations feed: what is out of the ordinary.
- `map` — The map over every entity with a location.
- `group` — Several widgets as one card, such as a dial with the traces that explain it, or a setpoint beside what it drives. Groups do not nest.
- `burner` — A burner's card: its two commands and the two temperatures an interlock reads. It uses only the `burner` vocabulary, with nothing adapter-specific.

## Capability vocabulary

What an entity of each capability publishes under which names (docs/adapters.md, State). The base aspect is what commands target and the dashboard widget renders; the other named aspects are what `features` may declare or the vocabulary reserves; notable is the reading that counts as a deviation on `Now`. Anything else an adapter publishes passes through under its native name.

| Capability | Base aspect | Other named aspects | Notable | Notes |
|---|---|---|---|---|
| `binary_sensor` | — | — | — | A boolean under its native name. |
| `burner` | `on` | `power_level`, `flue_temperature`, `boiler_temperature` | — | `on` is the family lever, read back from the device's run state, never echoed from the command. `power_level` is the output setting as the device enumerates it (a constraint the adapter describes); the two temperatures in °C are what an interlock reads. Run-phase codes pass through raw until a second burner adapter exists to generalise against. |
| `camera` | — | `motion` | — | `motion` (bool). Media rides the go2rtc plane, never the bus. |
| `climate` | `setpoint` | `indoor_temperature`, `feed_temperature` | — | `setpoint` in °C is the family lever; the two readings are normalized when the device has them. |
| `cover` | — | — | — | Reserved; no adapter binds it yet. |
| `light` | `on` | `brightness`, `color_temp` | `on = true` | `brightness` 0–254 (the Zigbee2MQTT scale the dashboard assumes), `color_temp` in mired. |
| `lock` | `locked` | — | `locked = false` |  |
| `notifier` | `message` | `alert`, `delivered` | — | A channel that reaches a person: a phone, a group chat. `message` and `alert` are commandable strings — the text itself — and two structurally separate delivery paths, granted and policed apart (an alert overrides quiet hours; a message never will). `delivered` is the epoch time the delivery service acknowledged the last message, never a human's receipt. Room `person` for one person's channel, `global` for a group (docs/design.md#notifications). |
| `person` | — | `presence`, `lat`, `lon`, `accuracy`, `battery`, `fixed_at` | — | `presence` is whether the person is home (bool), published by whichever adapter knows — a geofence transition, a fused sighting; the dashboard's People tile reads it. Scalar position aspects; `fixed_at` is the fix's epoch timestamp. Room is always `person`. |
| `presence` | — | `occupancy`, `presence` | — | Either spelling is accepted; adapters pass their native one through. |
| `router` | — | `wan` | `wan = false` |  |
| `sensor` | — | — | — | Numeric aspects under descriptive names (`temperature`, `humidity`); widgets come from what is published. |
| `switch` | `on` | — | — |  |

Every capability may also publish:

- `available` — bool, published on transition by the owning adapter when the protocol has a real loss signal; `false` is notable (docs/design.md#availability).
- `{aspect}_valid` — bool beside a reading the device itself may stop trusting; the value stands, the flag says stale.
