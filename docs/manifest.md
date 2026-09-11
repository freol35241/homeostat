# Manifest reference

Generated from the manifest structs by `homeostat schema --markdown`; do not edit (a test refuses a stale copy). The same schema is served as JSON by `homeostat schema [unit|entity|zones]` and the MCP `schema` tool. Rules the validator enforces beyond the shape are named by their error code; `homeostat explain <code>` (or the MCP `explain` tool) has the paragraph for each. Reasoning lives in docs/design.md.

## Unit manifest (`units/<name>.toml`)

A unit manifest: `units/<name>.toml`. One schema, three kinds (adapter,
automation, service); which sections a kind accepts is stated on each
section, and the validator refuses a mismatch with `invalid-manifest`.

| Field | Type | Required | Description |
|---|---|---|---|
| `bus` | [BusSection](#bussection) | no | The unit's declared bus surface. A unit gets exactly this and nothing else: the SDK refuses a publish outside it. |
| `discovery` | [DiscoverySection](#discoverysection) | no | How an adapter reaches its backend. Required for adapters, allowed for services, refused for automations. |
| `entities` | [EntitiesSection](#entitiessection) | no | The entities this unit binds. Required for adapters, allowed for automations (virtual sensors), refused for services. |
| `naming` | [UnitNaming](#unitnaming) | no | Human names for voice and the dashboard. Dashboard quality is a function of naming hygiene. |
| `params` | table of name → [ParamSpec](#paramspec) | no | Live parameters, keyed by name (`home/config/{unit}/{param}`). Names become key segments (`invalid-name`). |
| `runtime` | [RuntimeSection](#runtimesection) | yes |  |
| `schema` | integer | yes | Contract version. Must be 1 (`unsupported-schema` otherwise). |
| `unit` | [UnitSection](#unitsection) | yes |  |

### BusSection

`[bus]`: the unit's declared key surface. Keys are `home/{class}/...`
(`key-outside-schema` otherwise); a zone name in the room slot expands
to its rooms at plan time; `{room}`/`{entity}` templates expand per
bound entity and are valid only in adapters and automations
(`template-outside-binding-unit`).

| Field | Type | Required | Description |
|---|---|---|---|
| `publishes` | table of name → [PublishSpec](#publishspec) | no | `[bus.publishes]`: binding name → what the unit may publish. This is what the plan resolves into the grant table. |
| `subscribes` | table of name → string | no | `[bus.subscribes]`: binding name → key expression. The SDK subscribes by binding name. A unit's own `home/config/{unit}/*` is implicit and never declared. |

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
| `constraint` | table | no | Inline table of constraint keys the type understands (`malformed-constraint` otherwise): `min`/`max` for `int` and `float`; `after`/`before` (`"HH:MM"`, may span midnight) for `time`. |
| `default` | any | yes | A TOML literal of the declared type (`invalid-default` otherwise): `true`, `30`, `0.5` (an integer literal is accepted for `float`), `"text"`, or `"22:00"` for `time`. Must satisfy the constraint. |
| `editable_by` | [EditableBy](#editableby) | no | Who may change it live. `family` params appear as editable setpoints on the dashboard; anything else is visible there but written only through the repo (a propose) or the bus. |
| `type` | [ParamType](#paramtype) | yes |  |

### RuntimeSection

`[runtime]`: how the supervisor runs the unit.

| Field | Type | Required | Description |
|---|---|---|---|
| `command` | string | yes | Shell command, run from the house root with `HOMEOSTAT_UNIT` and `HOMEOSTAT_BUS` set. Typically `uv run units/<name>.py`. |
| `restart` | [RestartPolicy](#restartpolicy) | yes |  |
| `shutdown_grace_s` | integer | no | Seconds between SIGTERM and SIGKILL at shutdown. Default 5. |

### UnitSection

`[unit]`: identity.

| Field | Type | Required | Description |
|---|---|---|---|
| `description` | string | no |  |
| `inputs` | [UnitInputs](#unitinputs) | no | Which repo files are this unit's inputs. Absent means `own` — the unit's command, its own entity files, its zone if it uses one. |
| `kind` | [UnitKind](#unitkind) | yes |  |
| `name` | string | yes | Unique across the house; a bus key segment (`home/health/{unit}`, `home/config/{unit}/*`), so letters, digits, `_`, `-`, `.` only. `system` is reserved for the core. |

### PublishSpec

One publish the unit is allowed.

| Field | Type | Required | Description |
|---|---|---|---|
| `capability` | string | no | Required under `home/cmd/` (`publish-missing-capability`): the grant resolves onto bound entities of this capability that the key covers. Must be a known capability (`unknown-capability`). |
| `key` | string | yes | Key expression. Under `home/state/` it must name a bound entity's room and entity literally or by template (`state-publish-unbound`). |
| `priority` | [Priority](#priority) | no | The band commands leave at. Automations publish at `automation`; the family's surfaces (dashboard, voice) at `manual`, which always wins in arbitration. |

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

### UnitInputs

A unit whose model spans the WHOLE house (the dashboard) is changed by
any entity or manifest anywhere, not just by files it owns. Per-unit
change detection cannot infer that, so the unit declares it: otherwise
`apply` reports success while leaving the unit confidently stale.

- `own` — The unit's own files: its command, its entity files, its zone.
- `house` — Every manifest and entity file in the house.

### UnitKind

What a unit is, which decides the sections it may carry and how it is
ordered in an apply walk.

- `adapter` — Puts devices on the bus: binds entities, needs `[discovery]` and `[entities]`, publishes their state, takes their commands.
- `automation` — Regulates: subscribes state, publishes commands at the automation band, and may bind read-only virtual entities via `[entities]`.
- `service` — Infrastructure with no entities (the recorder, the dashboard, the arbiter, the clock). May carry `[discovery]`.

### Priority

Command priority bands, lowest to highest.

- `automation`
- `agent`
- `family`
- `manual`

## Entity file (`<entities dir>/<name>.toml`)

An entity file: `<entities dir>/<name>.toml`. The file stem is the
entity's name, unique across the house (`duplicate-entity-name`), and
a key segment (`invalid-name`).

| Field | Type | Required | Description |
|---|---|---|---|
| `dashboard` | [EntityDashboard](#entitydashboard) | no | `[dashboard]`: presentation hints for the family surface. Text in the house repo, never browser-side state (docs/design.md, Dashboard). |
| `entity` | [EntitySection](#entitysection) | yes |  |
| `inputs` | table of name → [InputSource](#inputsource) | no | `[inputs]`: device inputs fed from one source each, keyed by the adapter's own input name (e.g. `indoor_temperature_actual`). A fed input is a continuous signal with one master, not a command: it stops being a command aspect for this entity, never rides the arbiter, and staleness is the device's own validity window. Only a device entity can be fed (`virtual-entity-fed`); the adapter is the authority on which input names exist. |
| `naming` | [EntityNaming](#entitynaming) | no |  |
| `schema` | integer | yes | Contract version. Must be 1. |
| `write_policy` | [WritePolicy](#writepolicy) | yes |  |

### EntityDashboard

`[dashboard]` on an entity.

| Field | Type | Required | Description |
|---|---|---|---|
| `pin` | boolean | no | Pin this entity's numeric readings as signal tiles at the top of `Now`, each with today's range. Nothing is pinned by default: `Now` is the error signal, and a reading earns a place there by being named here. |

### EntitySection

`[entity]`: what the device is and where.

| Field | Type | Required | Description |
|---|---|---|---|
| `capability` | string | yes | One of: binary_sensor, burner, camera, climate, cover, light, lock, notifier, person, presence, router, sensor, switch, vpn (`unknown-capability`). Decides the base aspect, the dashboard widget and which cmd grants apply. |
| `features` | list of string | no | Optional aspects beyond the capability's base, as the adapter names them (`brightness`, `color_temp` on a light). For a sensor it is descriptive only; its widgets come from the numeric aspects it publishes. |
| `id` | string | yes | The adapter-native address (a zigbee2mqtt friendly name, an ESPHome node, a camera's go2rtc stream). Unique per adapter (`duplicate-entity-id`). |
| `room` | string | yes | The single source of spatial truth for this entity. A key segment; not `home` or a key class (`reserved-room-name`). The pseudo-rooms `global` and `person` are for entities with no place. |

### InputSource

The source of a fed input: an entity and one of its aspects, i.e. the
state key `home/state/{room}/{entity}/{aspect}`. The plan resolves it
and prints the edge, as it does a grant.

| Field | Type | Required | Description |
|---|---|---|---|
| `aspect` | string | yes | The aspect to read. When the source is automation-owned, that automation's `[bus.publishes]` must cover the key (`input-unpublished-aspect`). |
| `entity` | string | yes | Name of the source entity; must exist (`input-unknown-entity`). Any owner will do — an automation's virtual sensor or another adapter's device. |

### EntityNaming

`[naming]` on an entity.

| Field | Type | Required | Description |
|---|---|---|---|
| `aliases` | list of string | no | Alternative spoken names. |
| `en` | string | no | English label; the dashboard falls back to the name with `_` replaced by spaces. |
| `sv` | string | no | Swedish label. |

### WritePolicy

`[write_policy]`: who may command the entity and how conflicts resolve.

| Field | Type | Required | Description |
|---|---|---|---|
| `mode` | [WriteMode](#writemode) | yes |  |
| `owner` | string | yes | Exactly one unit binds each entity: an adapter, or an automation for virtual entities. Must exist (`missing-owner-unit`) and be the unit whose entities dir holds this file (`owner-mismatch`). |

### WriteMode

How commands toward the entity are governed. An automation-owned
(virtual) entity is a latch when commanded: `arbitrated` is refused
(`virtual-entity-arbitrated`), and a cmd grant may cover it only when
the owner subscribes to its cmd keys (`virtual-entity-commanded`).

- `shared` — Any granted writer may command it; last write wins.
- `exclusive` — At most one automation-band writer may be granted (`exclusive-write-conflict`); manual-band surfaces sit above.
- `arbitrated` — Commands go through the arbiter (leases, bands, preemption); the house must bind an arbiter-class publish covering it (`arbitrated-uncovered`).

## Zones (`zones.toml`)

`zones.toml` at the house root: named sets of rooms that expand in the
room slot of key expressions.

| Field | Type | Required | Description |
|---|---|---|---|
| `schema` | integer | yes | Contract version. Must be 1. |
| `zones` | table of name → list of string | no | `[zones]`: zone name → member rooms. A zone name is a key segment, not reserved (`reserved-zone-name`), not also a room (`zone-room-collision`); members must be rooms some entity binds (`zone-unknown-room`) and never pseudo-rooms (`zone-pseudo-room`). |

## Capability vocabulary

What an entity of each capability publishes under which names (docs/adapters.md, State). The base aspect is what commands target and the dashboard widget renders; the other named aspects are what `features` may declare or the vocabulary reserves; notable is the reading that counts as a deviation on `Now`. Anything else an adapter publishes passes through under its native name.

| Capability | Base aspect | Other named aspects | Notable | Notes |
|---|---|---|---|---|
| `binary_sensor` | — | — | — | A boolean under its native name. |
| `burner` | `on` | `power_level`, `flue_temperature`, `boiler_temperature` | — | `on` is the family lever, read back from the device's run state, never echoed from the command. `power_level` is the output setting as the device enumerates it (a constraint the adapter describes); the two temperatures in °C are what an interlock reads. Run-phase codes pass through raw until a second burner adapter exists to generalise against (#37). |
| `camera` | — | `motion` | — | `motion` (bool). Media rides the go2rtc plane, never the bus. |
| `climate` | `setpoint` | `indoor_temperature`, `feed_temperature` | — | `setpoint` in °C is the family lever; the two readings are normalized when the device has them. |
| `cover` | — | — | — | Reserved; no adapter binds it yet. |
| `light` | `on` | `brightness`, `color_temp` | `on = true` | `brightness` 0–254 (the Zigbee2MQTT scale the dashboard assumes), `color_temp` in mired. |
| `lock` | `locked` | — | `locked = false` |  |
| `notifier` | `message` | `alert`, `delivered` | — | A channel that reaches a person: a phone, a group chat. `message` and `alert` are commandable strings — the text itself — and two structurally separate delivery paths, granted and policed apart (an alert overrides quiet hours; a message never will). `delivered` is the epoch time the delivery service acknowledged the last message, never a human's receipt. Room `person` for one person's channel, `global` for a group (docs/design.md, Notifications). |
| `person` | — | `presence`, `lat`, `lon`, `accuracy`, `battery`, `fixed_at` | — | `presence` is whether the person is home (bool), published by whichever adapter knows — a geofence transition, a fused sighting; the dashboard's People tile reads it. Scalar position aspects; `fixed_at` is the fix's epoch timestamp. Room is always `person`. |
| `presence` | — | `occupancy`, `presence` | — | Either spelling is accepted; adapters pass their native one through. |
| `router` | — | `wan` | `wan = false` |  |
| `sensor` | — | — | — | Numeric aspects under descriptive names (`temperature`, `humidity`); widgets come from what is published. |
| `switch` | `on` | — | — |  |
| `vpn` | — | `up` | `up = false` |  |

Every capability may also publish:

- `available` — bool, published on transition by the owning adapter when the protocol has a real loss signal; `false` is notable (docs/design.md, Availability).
- `{aspect}_valid` — bool beside a reading the device itself may stop trusting; the value stands, the flag says stale.
