# Writing an adapter

This page is the contract every adapter follows. The reasoning is in
[design.md](design.md) and the history is in git. When this page and
design.md disagree, this page is out of date and should be fixed.

Each adapter documents itself in two places. Its module docstring says
what it does and how to set it up: the `id` binding rule, what the entity
file needs, its configuration (endpoint, environment variables,
parameters) and the health events it reports. Rules that govern one part
of the code, such as how a field is translated or when a command is
refused, are in the docstring or comment beside the code that applies
them.

An adapter is a unit that puts devices on the bus. It binds entity files
to things a protocol can reach, publishes their state and takes their
commands. It is a process, not a plugin: the supervisor starts it from a
command in a manifest, with the house repo as its working directory and
two environment variables. Anything that follows the contract below is
an adapter, in any language. The Python SDK (`sdk/python`) makes it
easier, every adapter in `adapters/` uses it, and the examples here
assume it.

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
policy, and the `home/arbiter` one only onto arbitrated entities. Declare
both: an adapter without the arbiter line cannot receive commands for an
arbitrated device.

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
serial number) and documents it in its module docstring. Agents write
entity files from discovery records and should not have to guess the
binding rule. Everything else in the entity file is the house's.

An adapter is named for the dialect it speaks, not the vendor that sells
the device: `onvif`, not `tapo`; `openwrt`, not the router's brand. Vendor
facts (a host, a port, a default path) are per-device configuration. A
vendor adapter is written only when the vendor's own API is needed for
something the dialect cannot do. Since one adapter binds each entity, it
then takes those devices over through an ordinary entity-file change.

## 2. Lifecycle

What the supervisor does, and what it expects back:

- **Spawn.** `runtime.command` is split on whitespace and exec'd with no
  shell. The working directory is the house root. The environment has
  `HOMEOSTAT_UNIT` (the unit name), `HOMEOSTAT_BUS` (the Zenoh endpoint,
  e.g. `tcp/127.0.0.1:7447`) and a fixed base set (`PATH`, `HOME`,
  locale, `TZ`, `UV_*`, `PYTHON*`, CA bundles). Nothing else from the
  supervisor's environment is passed unless the manifest declares it: a
  unit lists every variable it reads by name in `runtime.env`
  (`env = ["HOMEOSTAT_NTFY_TOKEN"]`), including the ones its `${VAR}`
  endpoint expands. A secret given to the supervisor for one unit is not
  visible to another. A `uv run <script>` command is resolved to the
  script's interpreter and exec'd directly.
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

- **Ready.** Call `session.ready()` once both translation directions are
  wired: the protocol connection is up and subscribed, and the command
  subscriptions are declared. The supervisor treats the liveliness token
  it declares, not the process, as "running". Until then the unit is
  `starting`, and a unit that never calls it never becomes healthy. Where
  the far end can be checked cheaply (an HTTP health endpoint), check it
  before `ready()`, so a wrong URL or a bad token shows as a startup
  failure instead of a unit that drops everything.
- **Proof of life.** A subscription that is accepted and matches nothing
  (a wrong topic prefix) raises no error, so the adapter hears nothing
  and reports healthy. Where the protocol delivers something on subscribe
  (a retained inventory, a retained state topic), report a health event
  naming what was subscribed if it has not arrived after a timeout. A
  topic that is republished only on change proves life at startup only.
  Its later silence says nothing, so liveness while running needs a
  signal that is periodic or stateful.
- **Foreign binaries** join through a thin SDK shim (`adapters/go2rtc.py`).
  The shim renders the binary's config, spawns it, waits until it
  answers, declares `ready()`, and exits when the child does, so the
  supervisor's backoff and process-group sweep apply as usual. The binary
  comes with the image, not the repo.
- **Stop.** The supervisor sends SIGTERM and waits `shutdown_grace_s`
  before SIGKILL. Undeclare subscribers, close the session, close the
  protocol connection, exit. `homeostat.mqtt.wait_for_shutdown()` is the
  blocking wait the MQTT adapters use.
- **Restart** follows `runtime.restart` with backoff and a circuit
  breaker. A misconfiguration that cannot be recovered (an unset `${VAR}`
  in the endpoint, an unknown input name in an entity file) should exit
  with a clear message; the backoff makes it visible on `Health`. Bad
  *input* at runtime must not make the adapter exit: see §6.

Configuration comes from the same files the core validated:

```python
from homeostat import connect, house, keys, mqtt

unit = os.environ[keys.ENV_UNIT]
config = house.load_adapter(unit)      # .endpoint (expanded), .entities
session = connect()                     # HOMEOSTAT_UNIT / HOMEOSTAT_BUS
```

Each entity carries `name`, `id`, `capability`, `features`, `room`,
`write_mode`, `owner`, `naming`, and `inputs` (§9). There is no other
config channel. Secrets stay out of the repo. An MQTT password goes in
the endpoint's `${VAR}` or in the TOML file that
`HOMEOSTAT_MQTT_CREDENTIALS` names, keyed by hostname. Per-device keys
go in a file that an adapter-specific variable names (as
`HOMEOSTAT_ESPHOME_DEVICES` and `HOMEOSTAT_CAMERAS` do). Every such
variable is declared in the unit's `runtime.env`, or the unit does not
see it (§2). Inline credentials in an endpoint URL take precedence over
the file. A raw `@`, `/` or `#` in a password changes how the URL parses
instead of failing, which is why the file exists. An HTTP client that
sends a secret (a login, a bearer token) refuses redirects, because a
redirect would replay the secret at whatever host the reply names.

## 3. State

Every reading is one key, one JSON scalar:

```
home/state/{room}/{entity}/{aspect}
```

- **Scalars only.** Numbers, booleans, strings. A composite reading is
  split into aspects (a position is `lat`, `lon`, `accuracy`, ...). The
  recorder stores only scalars, so anything else is missing from history.
- **One key segment per aspect.** A nested native path joins with
  underscores (`GT2/raw` → `GT2_raw`).
- **Normalize what the vocabulary names, pass the rest through.** The
  dashboard, the arbiter and the grant table act on each capability's
  base aspect, and `features` may declare the other named aspects. The
  table is generated from the schema and lives in
  [manifest.md, Capability vocabulary](manifest.md#capability-vocabulary),
  so it matches the core. Everything else publishes under its native
  field name. Adapter-native *values* are translated: `"ON"` becomes
  `true`, `LOCKED` becomes `locked = true`. A new device class that needs
  a new base aspect is a change to that table in `src/manifest.rs`, not
  an adapter convention.
- **The bus value is the device's readback**, not an echo of a command
  the adapter just forwarded.
- **Publish on change when the source repeats itself.** A bridge that
  republishes every field each poll, a camera that repeats a
  notification every evaluation tick, a router answering the same
  question each cycle: forward a value only when it differs from the
  last one put, and once after start (late joiners read the core's
  mirror). The recorder stores every put, so forwarding each poll
  records unchanged values at the poll's rate.
- **Do not invent values.** On loss the last values stand. The adapter
  publishes no nulls or zeros and clears no keys. Unknown is not false.
- **Validity travels beside the value.** A reading the device itself
  stops trusting publishes `{aspect}_valid` (bool) alongside `{aspect}`.
- **`available` is reserved** (§7). A passthrough that could mint it from
  a native field drops that field with a `reserved-aspect` event.

## 4. Commands

Every command payload is an envelope:

```json
{"value": 21.5, "priority": "manual", "actor": "dashboard"}
```

Subscribe on the expressions `keys.command_keyexprs(entity)`
returns: `home/cmd/{room}/{entity}/**` for a plain entity,
`home/arbiter/{room}/{entity}/**` for an arbitrated one. An arbitrated
entity has no `home/cmd` path at all; whatever arrives on the arbiter
class has already won its lease. The adapter ignores `priority`, which
is the arbiter's concern.

Per command:

1. Decode JSON; on failure drop with `malformed-payload`.
2. `keys.parse_cmd_envelope(payload)`; on `ValueError` drop with
   `invalid-command`. `session.parse_command(sample)` does steps 1-2 and
   hands back `(aspect, value, cmd_id)`; carry that `cmd_id` into every
   drop below, because each one ends a command someone is waiting on.
3. Check the aspect is one the entity takes: the capability's base
   aspect, its declared `features`, or a dialect knob this adapter
   documents. Anything else drops with `invalid-command`.
4. Check the value's type and bounds. **Bounds are adapter constants**,
   because device physics is dialect knowledge, not house config. An
   out-of-range value drops with `invalid-command`; do not clamp it.
5. Translate and send. Do not publish the new state yourself; wait for
   the device to report it.

A command toward an unavailable device is a per-adapter policy: drop with
`device-unavailable` if the protocol knows, or send and let the device
miss it.

**One master per device.** A device this adapter commands must have no
other writer. A Node-RED flow or a Home Assistant switch publishing to
the same set topic bypasses the arbiter and the audit trail, and neither
notices. Read-only consumers can coexist. When the dialect's ecosystem
commonly wires such writers, say so as an operational note in the module
docstring.

An adapter with nothing to command declares no command subscription.
Add a command surface when a command is needed, not because the protocol
could carry one.

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

- `id` is what an entity file's `id` must say. `configured` and `entity`
  say whether a file binds it, and which. `suggested` is a best-effort
  stanza in homeostat vocabulary, or null. `description` is optional raw
  protocol data for consumers that want more.
- `aspects` is optional and only on bound records. It is the entity's
  aspect descriptor, from which the dashboard learns labels, kinds,
  groups and the commands beyond the base vocabulary. Field shape:

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
  optional `step`, and `editable_by`. Only `family` commands are
  writable from the dashboard, and their constraint must match the
  adapter's own bounds. Undescribed aspects still render, in a
  diagnostics group. A fed input (§9) is described but carries no
  command.
- `readback_s` (seconds, optional) is how long the device can take to
  report a command back: on one command (`command.readback_s`) or for
  the whole entity, at the descriptor's top level beside `fields`. The
  dashboard waits that long before saying a command got no answer, and
  otherwise falls back to a guess per capability. Declare it when your
  protocol has a cadence you know: a bridge that polls (aduro: two polls
  and some slack), a firmware that publishes on a timer (ivt490: three
  cycles). Push protocols that answer at once (Zigbee2MQTT, ESPHome)
  can leave it out.

Generate the descriptor when the protocol already describes its devices.
The Zigbee2MQTT adapter derives one from each device's `exposes` (unit →
kind, category → group, settable config → owner-tier command), so no
per-device labels are written by hand. Write it by hand only for a
dialect with a fixed field list (the heat pump).

An adapter with nothing to enumerate publishes no discovery document.
An unbound device belongs in discovery and is not a dropped message:
report it once (`unknown-device`) or not at all, and not per publish.
When the inventory is whatever the wire carries (phones seen on a
broker, 433 MHz codes from the neighbours), cap the unbound records and
coalesce republishes, so a busy neighbourhood can neither grow the
document without bound nor republish it per message.

## 6. Health events

Dropped or degraded input does not crash the adapter, and it leaves a
trace: one JSON object at `home/health/{unit}/event` via
`session.health_event(kind, **fields)`. The parent key
`home/health/{unit}` belongs to the supervisor; do not write it.

| kind | reason / fields | when |
|---|---|---|
| `drop` | `reason = "malformed-payload"`, `topic` or `key` | undecodable input from either side |
| `drop` | `reason = "malformed-topic"`, `topic` (the raw bytes, `repr`-ed) | an MQTT topic that is not valid UTF-8; emitted by the SDK's guard, not the adapter |
| `drop` | `reason = "invalid-command"`, `key`, `cmd_id` | bad envelope, unknown aspect, wrong type, out of bounds |
| `drop` | `reason = "unknown-device"`, `topic` | a device outside the adapter's own view, first sight only |
| `drop` | `reason = "reserved-aspect"`, `topic` | a native field that would mint `available` |
| `drop` | `reason = "null-value"`, `topic` or `key` | a device reporting "no reading" as a null where a value belongs |
| `drop` | `reason = "device-unavailable"`, `key`, `cmd_id` | a command dropped because the device is down |
| `drop` | `reason = "invalid-feed"`, `input`, `key`, `value` | a fed value outside its bounds |
| `drop` | `reason = "feed-source-unavailable"`, `input`, `key` | a fed value arriving while its source is unavailable (once per outage) |
| `device-silent` / `bridge-silent` | `topic` or `base_topic`, `timeout_s` | a loss transition (once per transition) |
| `feed-source-lost` | `input`, `key` | a fed input's source went unavailable |

New kinds and reasons are fine (`mdns-unavailable`, `camera-misconfigured`
exist). Keep them to transitions and drops, not one per message, and list
them in the module docstring. A degraded condition (a silent bridge, an
unreachable router) gets its own event kind, not `drop`, since nothing
was dropped.

## 7. Availability

`available` (bool) at `home/state/{room}/{entity}/available` is the
device liveness signal, published on transition by the owning adapter.

- **Opt-in.** Publish it only if the protocol gives you a real loss
  signal: a bridge availability topic, a TCP session, a subscription
  that lapses, or a known publish cadence you can time. A retained
  last position with no liveness meaning publishes nothing.
- **On transition only**, with one health event per down transition.
- **Stale, not false.** Loss flips `available`; every other aspect keeps
  its last value.
- A silence timeout is a live parameter (`availability_timeout_s`,
  owner-editable) with an adapter-side default, so a house can tune it.

### One-way senders

A device that asserts and never retracts (433 MHz PIRs, door contacts,
doorbells) needs its off synthesized, and the adapter does that (see
[design.md](design.md#one-way-senders)). Publish `true` on the first
assertion and `false` when the hold expires, and extend the deadline on
a repeat burst without publishing. Publish `false` for every bound
entity at startup: the held value is the adapter's, not the device's, so
after a restart the correct state is "nothing has asserted". The hold is
a per-aspect live parameter, chosen by the entity's capability and
features. `adapters/rf433.py` is the worked example.

## 8. Live parameters

An adapter may declare `[params]` in its manifest: timeouts, poll
intervals, owner-editable. Read them with `homeostat.params.LiveParams`,
which subscribes to `home/config/{unit}/*`, seeds from the core's cache,
and holds adapter-side defaults so a manifest may omit any of them.
Parameters are tuning. Anything that changes what the adapter binds or
speaks is a manifest or entity-file change and goes through `plan`.

## 9. Device feeds (optional)

A device input that is a continuous signal with one master (a room
temperature the pump's controller consumes) is a feed, not a command.
The adapter declares which inputs are feedable and their bounds; the
entity file wires each to a source `{entity, aspect}`; the SDK resolves
it into `entity.inputs`. Rules:

- Forward each source sample within bounds (else `invalid-feed`), and
  nothing between samples. Subscribe the value and `available` keys
  through one subscriber (the source entity's
  `home/state/{room}/{entity}/*`). zenoh orders samples within a
  subscriber but not across two, and a value must not be dropped because
  the `available = true` just before it was delivered second.
- A wired input stops being a command aspect for that entity.
- Forward while the source is `available`. On loss, clear any retained
  slot once and report `feed-source-lost`. Do not retain a fed value.
- An unknown input name in an entity file is a startup error.

### Retaining a command

Whether a command is published retained is decided per aspect, by one
question: **does the device expire this value?**

- **It expires** (a timestamped control input, a validity window, a
  watchdog): **do not retain it.** Its writer's cadence keeps it
  current, and expiry is what lets a writer stop. An automation that
  goes quiet on stale inputs relies on the device dropping its value and
  falling back to a safe default. A retained copy is redelivered when
  the device reconnects and re-applied *after* the writer stopped, which
  leaves the device stuck on it. A fed input is the clearest case and is
  covered above.
- **It does not expire** (a stored setting, a mode, a setpoint):
  **retain it.** A setting set four days ago is as valid as one set a
  minute ago. Retaining it means a device that reboots to a firmware
  default, or that missed the write, gets the value back when it
  reconnects.

One device often has both kinds, so keep the flag beside each aspect's
other properties, not at the publish site. Test both in one run: a late
subscriber receives the retained aspect *and* does not receive the
expiring one. A test that only checks the absence passes when nothing
was published at all.

## 10. Testing

Adapters are tested end to end against the real supervisor and a real
protocol endpoint, without mocking the bus. The pattern, from
`tests/ivt490.rs`:

- A **fixture house** under `tests/fixtures/house_<adapter>/` with the
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
  exit inside `shutdown_grace_s`, no orphan). The unit contract is one
  call to `common::assert_unit_contract(&mut sup, &observer, "<unit>")`,
  the conformance check every adapter suite ends with.
- Run `uv sync --script adapters/<adapter>.py` before `cargo test` so
  dependency resolution does not eat into supervision timeouts.

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
- [ ] Module docstring states what the unit does, the `id` binding rule,
      its configuration and its health events. Translation rules,
      command bounds and failure policy are documented beside the code
      that applies them.
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
