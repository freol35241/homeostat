# Homeostat

A household regulator, after W. Ross Ashby's 1948 machine. The system
maintains a home in equilibrium; the family adjusts setpoints, the owner
governs structure. The repo is the single source of truth — no hidden state
mutated by a UI. See [docs/design.md](docs/design.md) for the full design
record.

## Status: build-sequence step 3

Step 1 (key space + manifest parser + validator, `homeostat plan`) and
step 2 (the process supervisor: `homeostat up` runs a house's units as
supervised OS processes on a Zenoh bus, with liveliness tokens, exponential
restart backoff, and a circuit breaker visible at `home/health/{unit}`) are
done. Step 3 adds the first real adapter — Zigbee2MQTT — and bootstraps the
Python SDK it consumes.

## Usage

```
cargo run -- plan examples/house
```

On a valid repo it prints the plan (below, verbatim). On an invalid repo it
prints the complete error list and exits non-zero.

```
Homeostat plan
  repo:  examples/house
  world: empty

Units to create (4):

+ adapter esphome (units/esphome.toml)
    command: uv run units/esphome.py
    entities (1):
      outdoor_temp  sensor  room=global  write=shared
+ adapter zigbee (units/zigbee.toml)
    command: uv run units/zigbee.py
    entities (4):
      front_door_lock  lock      room=hallway     write=arbitrated
      hallway_motion   presence  room=hallway     write=shared
      kitchen_ceiling  light     room=kitchen     write=shared
      livingroom_lamp  light     room=livingroom  write=exclusive
+ automation evening_lights (units/evening_lights.toml)
    command: uv run units/evening_lights.py
    params:
      off_time  type=time  default=23:00  constraint={after=20:00, before=02:00}  editable_by=family
+ service recorder (units/recorder.toml)
    command: uv run units/recorder.py

Expanded keys:

  esphome publishes state: home/state/{room}/{entity}/**
    -> home/state/global/outdoor_temp/**
  esphome subscribes commands: home/cmd/{room}/{entity}/**
    -> home/cmd/global/outdoor_temp/**
  zigbee publishes state: home/state/{room}/{entity}/**
    -> home/state/hallway/front_door_lock/**
    -> home/state/hallway/hallway_motion/**
    -> home/state/kitchen/kitchen_ceiling/**
    -> home/state/livingroom/livingroom_lamp/**
  zigbee subscribes commands: home/cmd/{room}/{entity}/**
    -> home/cmd/hallway/front_door_lock/**
    -> home/cmd/hallway/hallway_motion/**
    -> home/cmd/kitchen/kitchen_ceiling/**
    -> home/cmd/livingroom/livingroom_lamp/**
  evening_lights publishes lights: home/cmd/downstairs/**/light (zone downstairs)
    -> home/cmd/kitchen/**/light
    -> home/cmd/livingroom/**/light
    -> home/cmd/hallway/**/light
  evening_lights subscribes clock: home/clock/minute
  evening_lights subscribes presence: home/state/downstairs/**/presence (zone downstairs)
    -> home/state/kitchen/**/presence
    -> home/state/livingroom/**/presence
    -> home/state/hallway/**/presence
  recorder subscribes state: home/state/**

Grant table:

  evening_lights.lights  capability=light  priority=automation
    -> kitchen_ceiling  (room=kitchen, write=shared, owner=zigbee)
    -> livingroom_lamp  (room=livingroom, write=exclusive, owner=zigbee)

Plan tier: structural (4 units created, 1 grant added)
```

Everything is structural against an empty world; the tier derivation
(parameter-only / behavioral / structural, per the design record) lives in
`src/plan.rs` and any grant-table delta escalates the tier.

## up

`homeostat up` validates a house repo exactly like `plan`, refuses it on any
error, and otherwise runs every unit as a supervised process:

```
cargo build
PATH="$PWD/target/debug:$PATH" cargo run -- up tests/fixture_house
```

The supervisor opens a router-mode Zenoh session listening on
`tcp/127.0.0.1:7447` (override with `--listen`) — the hub all units and
observers connect to as clients — and hands each unit its name and the bus
endpoint via `HOMEOSTAT_UNIT` / `HOMEOSTAT_BUS`. Units declare a liveliness token at
`home/health/{unit}/alive` when ready; the supervisor publishes JSON health
at `home/health/{unit}` (`starting`, `running`, `backoff`, `open`,
`stopped`) and `home/meta/{unit}/manifest_hash`. Transitions are also logged
to stdout, so the fixture house shows the fake adapter going `starting` →
`running` with its pid.

Crashed units restart per their manifest `restart` policy with exponential
backoff (100ms doubling, 30s cap); five consecutive quick exits open the
circuit breaker (`status = "open"`, no further restarts). SIGTERM/Ctrl-C
terminates units gracefully within their `shutdown_grace_s`, then SIGKILLs
the process group — no orphans, even if the supervisor itself is SIGKILLed
(pdeathsig). The full contract is in
[docs/design.md](docs/design.md#supervision-settled-in-step-2).

The `PATH` prefix is only for the fixture house, whose fake adapter is the
`fake_adapter` binary built by this crate; real houses use commands that
resolve on their own (`uv run units/...`).

`tests/supervision.rs` pins the four supervision scenarios (spawn shows
liveliness and state, induced crash restarts with observable backoff, a
crash loop opens the breaker, SIGTERM shuts down cleanly without orphans)
against the real binary on `tests/fixture_house/`.

## Zigbee2MQTT adapter

`adapters/zigbee2mqtt.py` is the first Python unit: a uv-run script with
PEP 723 inline dependencies and the first consumer of the Python SDK at
`sdk/python/` (`homeostat.session` — bus session from
`HOMEOSTAT_UNIT`/`HOMEOSTAT_BUS` plus the liveliness token,
`homeostat.keys` — key builders, `homeostat.house` — manifest and entity
loading; the script pulls the SDK in via a `[tool.uv.sources]` path source).

It is a translating subscriber. Zigbee2MQTT state on `zigbee2mqtt/{id}`
fans out to per-aspect keys:

```
zigbee2mqtt/lamp_kitchen_1  {"state":"ON","brightness":128}
  -> home/state/kitchen/kitchen_lamp/on          true
  -> home/state/kitchen/kitchen_lamp/brightness  128
```

and commands translate back:

```
home/cmd/kitchen/kitchen_lamp/on  true
  -> zigbee2mqtt/lamp_kitchen_1/set  {"state":"ON"}
```

The entity file's `id` is the z2m topic segment; the file stem is the bus
entity name. The z2m `state` field is normalized (`on` for lights/switches,
`locked` for locks); other scalar fields pass through under their z2m
names. Lock entities are state-only — no command subscription until the
arbiter exists. Unknown devices and malformed payloads are dropped with a
JSON event at `home/health/{unit}/event`, never a crash. Full conventions
in [docs/design.md](docs/design.md#zigbee2mqtt-adapter-and-python-sdk-settled-in-step-3).

The adapter reads its own manifest and entity files from the house repo
(cwd is the house root); the MQTT endpoint comes from
`[discovery].endpoint`, with `${VAR}` env expansion done adapter-side. Try
it against a live broker (needs `mosquitto` and `uv`):

```
mosquitto -p 1883 &
uv sync --script adapters/zigbee2mqtt.py   # pre-warm the env (optional)
HOMEOSTAT_TEST_MQTT_PORT=1883 cargo run -- up tests/fixture_house_z2m
mosquitto_pub -t zigbee2mqtt/lamp_kitchen_1 -m '{"state":"ON","brightness":128}'
```

`tests/z2m.rs` pins four scenarios against a real mosquitto on a free port
and the real supervisor: state translation onto the Zenoh bus, command
translation onto MQTT (including lock-command silence), unknown-device and
malformed payloads dropped with health events, and the step-2 unit contract
(liveliness token, graceful SIGTERM within the grace). CI installs
mosquitto and uv and pre-warms the adapter environment before `cargo test`.

## House repo layout

`examples/house/` doubles as documentation:

```
examples/house/
  zones.toml              # named sets of rooms; zones never appear in keys
  units/
    zigbee.toml           # adapter: discovery, templated keys, entities dir
    esphome.toml          # adapter
    evening_lights.toml   # automation: subscribes, publishes, params
    recorder.toml         # service
  entities/
    zigbee/
      kitchen_ceiling.toml   # one file per device; file stem = entity name
      ...
    esphome/
      outdoor_temp.toml
```

The entity file is the sole authority on write policy; the file stem is the
globally unique entity name; `room` in the entity file is the single source
of spatial truth.

## Validation

Plan-time validation covers: globally unique unit and entity names,
key-space schema conformance (`home/{class}/{room}/{entity}/{aspect}`),
capability matching between publishes and entities, write-policy enforcement
(two writers on an exclusive entity is an error), the reserved pseudo-room
list (`global`, `person`), exactly one owner adapter per entity, zone
well-formedness, and parameter constraint well-formedness (min/max,
after/before, enum).

The test corpus in `tests/corpus/invalid/` pairs each invalid house with its
complete expected error list; `tests/corpus/expected_plan.txt` pins the plan
output above.

```
cargo test
```
