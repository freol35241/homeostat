# Writing an adapter

The normative contract for an adapter, stated as it is today. The design
record ([design.md](design.md)) holds the reasoning and the history; this
page holds the rules. When they disagree, this page is stale — fix it.

An adapter is a unit that puts devices on the bus: it binds entity files
to things a protocol can reach, publishes their state, and takes their
commands. It is not a plugin. It is a process the supervisor starts from
a command in a manifest, with the house repo as its working directory
and two environment variables. Anything that honours the contract below
is an adapter, in any language. The Python SDK (`sdk/python`) is the
convenient way, and every adapter in `adapters/` uses it; the examples
here assume it.

## 1. Files

Three things in the house repo, all hashed by `plan`/`apply`:

```
units/
  acme.toml            # the manifest
  acme.py              # the code (or any command on PATH)
entities/
  acme/
    kitchen_lamp.toml  # one file per bound device; the stem is the entity name
```

The manifest ([manifest.md](manifest.md) is the field reference):

```toml
schema = 1

[unit]
name = "acme"
kind = "adapter"
description = "ACME bridge adapter"

[runtime]
command = "uv run units/acme.py"
restart = "always"
shutdown_grace_s = 5

[discovery]
mode = "static"                      # or "mdns" with service = "_acme._tcp"
endpoint = "mqtt://mosquitto:1883"   # opaque to the core; ${VAR} expands at start

[bus.publishes]
state = { key = "home/state/{room}/{entity}/**" }
discovery = { key = "home/discovery/acme" }

[bus.subscribes]
commands = "home/cmd/{room}/{entity}/**"
arbiter_commands = "home/arbiter/{room}/{entity}/**"

[entities]
dir = "entities/acme/"
```

`{room}` and `{entity}` expand per bound entity at plan time. The
`home/cmd` subscription expands only onto entities with a plain write
policy; the `home/arbiter` one only onto arbitrated entities. Declare
both — an adapter that omits the arbiter line cannot receive commands for
an arbitrated device, by construction.

An entity file:

```toml
schema = 1

[entity]
id = "0x00158d0001a2b3c4"   # the adapter-native address: yours to define
capability = "light"
features = ["brightness"]
room = "kitchen"

[naming]
en = "kitchen lamp"

[write_policy]
mode = "shared"             # shared | exclusive | arbitrated
owner = "acme"
```

The adapter defines what `id` means (a topic segment, a hostname, a
serial number) and documents it in its module docstring, because agents
construct entity files from discovery records and must never guess the
binding rule. Everything else in the entity file is the house's.

## 2. Lifecycle

What the supervisor does, and what it expects back:

- **Spawn.** `runtime.command` is whitespace-tokenized and exec'd with no
  shell. Working directory is the house root. Environment carries
  `HOMEOSTAT_UNIT` (the unit name) and `HOMEOSTAT_BUS` (the Zenoh
  endpoint, e.g. `tcp/127.0.0.1:7447`). A `uv run <script>` command is
  resolved to the script's interpreter and exec'd directly.
- **Dependencies** live in the script's PEP 723 block. Inside this repo an
  adapter uses a path source so tests exercise the working-tree SDK; a
  shipped copy in a house pins the release wheel and carries no sources
  block (the image bundles the wheel):

  ```python
  # /// script
  # requires-python = ">=3.11"
  # dependencies = ["homeostat", "paho-mqtt>=2,<3"]
  #
  # [tool.uv.sources]
  # homeostat = { path = "../sdk/python", editable = true }
  # ///
  ```

- **Ready.** Call `session.ready()` only once both translation directions
  are wired: the protocol connection is up and subscribed, and the
  command subscriptions are declared. The liveliness token it declares is
  what "running" means to the supervisor, not the process. Before it,
  the unit is `starting`; a unit that never calls it never becomes
  healthy.
- **Stop.** The supervisor sends SIGTERM and waits `shutdown_grace_s`
  before SIGKILL. Undeclare subscribers, close the session, close the
  protocol connection, exit. `homeostat.mqtt.wait_for_shutdown()` is the
  blocking wait the MQTT adapters use.
- **Restart** follows `runtime.restart` with backoff and a circuit
  breaker. A misconfiguration that cannot be recovered (an unset `${VAR}`
  in the endpoint, an unknown input name in an entity file) should exit
  with a clear message — the backoff makes it visible on `Health`. Bad
  *input* at runtime must never exit: see §6.

Configuration comes from the same files the core validated:

```python
from homeostat import connect, house, keys, mqtt

unit = os.environ[keys.ENV_UNIT]
config = house.load_adapter(unit)      # .endpoint (expanded), .entities
session = connect()                     # HOMEOSTAT_UNIT / HOMEOSTAT_BUS
```

Each entity carries `name`, `id`, `capability`, `features`, `room`,
`write_mode`, `owner`, `naming`, and `inputs` (§9). There is no second
config channel. Secrets never enter the repo: an MQTT password goes in
the endpoint's `${VAR}` or in the TOML that `HOMEOSTAT_MQTT_CREDENTIALS`
names; per-device keys go in a file an adapter-specific variable names
(`HOMEOSTAT_ESPHOME_DEVICES` is the precedent).

## 3. State

Every reading is one key, one JSON scalar:

```
home/state/{room}/{entity}/{aspect}
```

- **Scalars only.** Numbers, booleans, strings. A composite reading is
  split into aspects (a position is `lat`, `lon`, `accuracy`, ...). The
  recorder stores scalars; anything else is invisible to history.
- **One key segment per aspect.** A nested native path joins with
  underscores (`GT2/raw` → `GT2_raw`).
- **Normalize what the vocabulary names, pass the rest through.** The
  base aspect per capability is what the dashboard, the arbiter and the
  grant table act on; the other named aspects are what `features` may
  declare. The table is generated from the schema and lives in
  [manifest.md, Capability vocabulary](manifest.md#capability-vocabulary)
  — one source, so it cannot drift from the core. Everything else
  publishes under its native field name. Adapter-native *values* never
  leak: `"ON"` becomes `true`, `LOCKED` becomes `locked = true`. A new
  device class that needs a new base aspect is a change to that table in
  `src/manifest.rs`, not an adapter convention.
- **The bus value is the device's readback**, never an echo of a command
  the adapter just forwarded.
- **Never invent.** On loss the last values stand; the adapter publishes
  no nulls, no zeros, and clears no keys. Unknown is not false.
- **Validity travels beside the value.** A reading the device itself
  stops trusting publishes `{aspect}_valid` (bool) alongside `{aspect}`.
- **`available` is reserved** (§7). A passthrough that could mint it from
  a native field drops that field with a `reserved-aspect` event.

## 4. Commands

Every command payload is an envelope:

```json
{"value": 21.5, "priority": "manual", "actor": "dashboard"}
```

Subscribe on exactly the expressions `keys.command_keyexprs(entity)`
returns: `home/cmd/{room}/{entity}/**` for a plain entity,
`home/arbiter/{room}/{entity}/**` for an arbitrated one. An arbitrated
entity has no `home/cmd` path at all; whatever arrives on the arbiter
class has already won its lease. The adapter does not look at
`priority`; it is the arbiter's business.

Per command:

1. Decode JSON; on failure drop with `malformed-payload`.
2. `keys.parse_cmd_envelope(payload)`; on `ValueError` drop with
   `invalid-command`.
3. Check the aspect is one the entity takes: the capability's base
   aspect, its declared `features`, or a dialect knob this adapter
   documents. Anything else drops with `invalid-command`.
4. Check the value's type and bounds. **Bounds are adapter constants**
   (device physics is dialect knowledge, not house config); an
   out-of-range value drops with `invalid-command`. Never clamp
   silently.
5. Translate and send. Do not publish the new state yourself; wait for
   the device to report it.

A command toward an unavailable device is a per-adapter policy: drop with
`device-unavailable` if the protocol knows, or send and let the device
miss it.

## 5. Discovery

An adapter that can enumerate its periphery publishes one JSON array at
`home/discovery/{unit}`, the whole inventory each time it changes, and
declares that key in `[bus.publishes]`. One record per device:

```json
{
  "id": "0x00158d0001a2b3c4",
  "configured": true,
  "entity": "kitchen_lamp",
  "bound": true,
  "suggested": {"capability": "light", "features": ["brightness"]},
  "description": {"...": "the raw protocol descriptor, verbatim"},
  "aspects": {"schema": 1, "groups": ["control", "readings"], "fields": {"...": "..."}}
}
```

- `id` is exactly what an entity file's `id` must say. `configured` and
  `entity` say whether a file binds it, and which. `suggested` is a
  best-effort stanza in homeostat vocabulary, or null. `description` is
  optional raw material for richer consumers.
- `aspects` is optional and only on bound records: the entity's aspect
  descriptor, which is how the dashboard learns labels, kinds, groups and
  the commands beyond the base vocabulary. Field shape:

  ```json
  "operating_mode": {
    "label": "mode", "kind": "enum", "group": "control",
    "values": [{"value": 1, "label": "normal"}, {"value": 3, "label": "boost"}],
    "command": {"type": "enum", "editable_by": "family"}
  },
  "indoor_temperature": {"label": "indoor", "kind": "temperature", "group": "readings",
                         "valid": "indoor_temperature_valid"},
  "alarm": {"label": "alarm", "kind": "boolean", "group": "readings", "notable": true}
  ```

  `kind` is one of `temperature`, `temperature_delta`, `percent`,
  `number` (with an optional `unit` string the page shows after the
  value), `boolean` (optionally with `values` naming true and false,
  "locked"/"unlocked"), `enum`, `text`. `command` uses the manifest's ParamSpec
  fields: `type` (`float`, `int`, `enum`), `constraint` (`min`/`max`),
  optional `step`, and `editable_by` — only `family` commands are
  writable from the dashboard, and their constraint must match the
  adapter's own bounds. Undescribed aspects still render, in a
  diagnostics group. A fed input (§9) is described but carries no
  command.

Generate the descriptor when the protocol already describes its devices:
the Zigbee2MQTT adapter derives one from each device's `exposes` (unit →
kind, category → group, settable config → owner-tier command), so no
per-device labels are hand-written. Hand-write it only for a dialect with
a fixed field list (the heat pump).

An adapter with nothing to enumerate publishes no discovery document.
An unbound device is a discovery fact, not a dropped message: report it
once (`unknown-device`) or not at all, never per publish.

## 6. Health events

Dropped or degraded input never crashes the adapter and always leaves a
trace: one JSON object at `home/health/{unit}/event` via
`session.health_event(kind, **fields)`. The parent key
`home/health/{unit}` is the supervisor's; never write it.

| kind | reason / fields | when |
|---|---|---|
| `drop` | `reason = "malformed-payload"`, `topic` or `key` | undecodable input from either side |
| `drop` | `reason = "invalid-command"`, `key` | bad envelope, unknown aspect, wrong type, out of bounds |
| `drop` | `reason = "unknown-device"`, `topic` | a device outside the adapter's own view, first sight only |
| `drop` | `reason = "reserved-aspect"`, `topic` | a native field that would mint `available` |
| `drop` | `reason = "device-unavailable"`, `key` | a command dropped because the device is down |
| `drop` | `reason = "invalid-feed"`, `input`, `key`, `value` | a fed value outside its bounds |
| `drop` | `reason = "feed-source-unavailable"`, `input`, `key` | a fed value arriving while its source is unavailable (once per outage) |
| `device-silent` / `bridge-silent` | `topic` or `base_topic`, `timeout_s` | a loss transition (once per transition) |
| `feed-source-lost` | `input`, `key` | a fed input's source went unavailable |

New kinds and reasons are fine (`mdns-unavailable`, `camera-misconfigured`
exist); keep them to transitions and drops, never per message, and name
them in the module docstring.

## 7. Availability

`available` (bool) at `home/state/{room}/{entity}/available` is the
device liveness signal, published on transition by the owning adapter.

- **Opt-in.** Publish it only if the protocol gives you a real loss
  signal: a bridge availability topic, a TCP session, a subscription
  that lapses, or a known publish cadence you can time. A retained
  last-position with no liveness semantics publishes nothing.
- **On transition only**, with one health event per down transition.
- **Stale, not false.** Loss flips `available`; every other aspect keeps
  its last value.
- A silence timeout is a live parameter (`availability_timeout_s`,
  owner-editable) with an adapter-side default, never a constant a house
  cannot tune.

### One-way senders

A device that asserts and never retracts (433 MHz PIRs, door contacts,
doorbells) needs its off synthesized, and that is the adapter's job —
see docs/design.md, One-way senders. Publish `true` on the first
assertion and `false` when the hold expires, extend the deadline on a
repeat burst without publishing, and publish `false` for every bound
entity at startup: the held value is yours, not the device's, so after a
restart the honest state is "nothing has asserted". The hold is a
per-aspect live parameter, chosen by the entity's capability and
features. `adapters/rf433.py` is the worked example.

## 8. Live parameters

An adapter may declare `[params]` in its manifest — timeouts, poll
intervals, owner-editable. Read them with `homeostat.params.LiveParams`,
which subscribes to `home/config/{unit}/*`, seeds from the core's cache,
and holds adapter-side defaults so a manifest may omit any of them.
Parameters are tuning; anything that changes what the adapter binds or
speaks is a manifest or entity-file change and goes through `plan`.

## 9. Device feeds (optional)

A device input that is a continuous signal with one master (a room
temperature the pump's controller consumes) is a feed, not a command.
The adapter declares which inputs are feedable and their bounds; the
entity file wires each to a source `{entity, aspect}`; the SDK resolves
it into `entity.inputs`. Rules:

- Forward each source sample within bounds (else `invalid-feed`); nothing
  between samples. Subscribe the value and `available` keys through ONE
  subscriber (the source entity's `home/state/{room}/{entity}/*`): zenoh
  orders samples within a subscriber, not across two, and a value must
  never be dropped because the `available = true` just before it was
  delivered second.
- A wired input stops being a command aspect for that entity.
- Forward while the source is `available`; on loss, clear any retained
  slot once and report `feed-source-lost`. Never retain a fed value.
- An unknown input name in an entity file is a startup error.

## 10. Testing

Adapters are tested end to end against the real supervisor and a real
protocol endpoint, never with mocks of the bus. The pattern, from
`tests/ivt490.rs`:

- A **fixture house** under `tests/fixture_house_<adapter>/` with the
  manifest pointing at `../../adapters/<adapter>.py`, an endpoint that
  reads a port from `${VAR}`, and one or two entity files.
- `tests/common` provides `Supervisor::spawn_with_env`, `Mosquitto` (a
  real broker on a free port) and `Mqtt` (a scripted client),
  `expect_states`, `expect_drop_event`, `expect_event_kind`,
  `health_watch` / `await_health`, and `process_alive`.
- Cover, at minimum: state translation (normalized and passthrough,
  nested and malformed input, nothing from unsubscribed topics); the
  command path (an enveloped command reaches the device, an envelope-less
  or out-of-range one drops with the right event and nothing reaches the
  device); availability if published; the discovery document and its
  descriptor; and the unit contract (liveliness when ready, clean SIGTERM
  exit inside `shutdown_grace_s`, no orphan) — one call to
  `common::assert_unit_contract(&mut sup, &observer, "<unit>")`, the
  conformance check every adapter suite ends with.
- Run `uv sync --script adapters/<adapter>.py` before `cargo test` so
  dependency resolution never eats into supervision timeouts.

## 11. Shipping

Generic adapters live in `adapters/` in this repo. A house gets a copy
in its `units/`, pinned to the release's SDK; `scripts/sync_starter.sh`
regenerates the starter house's copies at each tag. A house-specific
adapter simply lives in that house's `units/` and pins the SDK the same
way. There is no registry and no loading step: a file in `units/` with
a manifest beside it is installed, and `plan` shows it.

## Checklist

- [ ] Manifest: `kind = "adapter"`, `[discovery]`, `[entities]`, state
      publish, both command subscriptions, discovery publish if any.
- [ ] Module docstring states the `id` binding rule, the aspects
      published, the commands taken with bounds, the health vocabulary.
- [ ] `load_adapter` for configuration; secrets outside the repo.
- [ ] Scalars, one key per aspect; base aspects normalized; readback,
      not echo; nothing invented on loss.
- [ ] Commands via `command_keyexprs` + `parse_cmd_envelope`; type and
      bounds checked; drops leave events.
- [ ] `available` if there is a real loss signal; timeout as a parameter.
- [ ] Discovery document with `id`, `configured`, `entity`, `suggested`,
      and an `aspects` descriptor for bound entities.
- [ ] `ready()` after both directions are wired; clean exit on SIGTERM.
- [ ] Fixture house and integration tests covering §10.
