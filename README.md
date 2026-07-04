# Homeostat

A household regulator, after W. Ross Ashby's 1948 machine. The system
maintains a home in equilibrium; the family adjusts setpoints, the owner
governs structure. The repo is the single source of truth — no hidden state
mutated by a UI. See [docs/design.md](docs/design.md) for the full design
record.

## Status: build-sequence step 5a

Step 1 (key space + manifest parser + validator, `homeostat plan`),
step 2 (the process supervisor: `homeostat up` runs a house's units as
supervised OS processes on a Zenoh bus, with liveliness tokens, exponential
restart backoff, and a circuit breaker visible at `home/health/{unit}`),
step 3 (the Zigbee2MQTT adapter and the Python SDK bootstrap) and step 4
(the first automation — `evening_lights` — the clock service, and the live
parameter path end to end: `home/config/{unit}/{param}` backed by a
core-owned last-value cache, constraint-validated writes, and edits that
reach running units without a restart) are done. Step 5a adds the recorder:
history end to end — typed state/cmd samples and an audit trail in a SQLite
store, with history reads served over the bus at `home/history/**`.

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
  recorder publishes history: home/history/**
  recorder subscribes cmd: home/cmd/**
  recorder subscribes config: home/config/**
  recorder subscribes health: home/health/**
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
the process group — the grace applies to the whole group, and a unit whose
direct child exits on its own gets its group swept before the restart, so
a `uv run` wrapper can't leave its interpreter behind. pdeathsig covers a
SIGKILLed supervisor. The full contract is in
[docs/design.md](docs/design.md#supervision-settled-in-step-2).

Health is published on transitions and served to late joiners by a
queryable from the core's last-value cache (get `home/health/{unit}` for
the current state).

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

## Automations and parameters

`tests/fixture_house_evening/units/evening_lights.py` is the first
automation: a regulator, not a scheduler. On every input — clock minute,
presence change, light state — it re-evaluates one rule: inside the night
window with nobody present, every light that is on gets turned off. It is
built on the SDK's automation Context, which gives a unit exactly the
surface its manifest declares:

```python
from homeostat import automation

ctx = automation.context()            # reads units/{HOMEOSTAT_UNIT}.toml
ctx.subscribe("presence", on_presence)  # binding names from [bus.subscribes];
ctx.subscribe("clock", on_clock)        # handlers get (key, decoded JSON)
ctx.params.off_time                   # typed (datetime.time), live-updated
ctx.publish("lights", False, room="livingroom", entity="lamp")
                                      # through [bus.publishes]; concrete
                                      # keys only, checked against the
                                      # declared expression
ctx.ready(); ctx.run()                # liveliness token, block until SIGTERM
```

Zone references in subscribe/publish expressions expand against the
house's `zones.toml`, the same expansion the core performs at plan time.

Parameters live on the bus at `home/config/{unit}/{param}`, seeded from
manifest defaults and backed by a core-owned last-value cache. A zenoh GET
without payload reads the current value; a GET **with** payload is a write:
the core validates it against the manifest's type and constraint (min/max,
after/before with midnight spanning, enum) and either stores + publishes it
— every subscribed unit sees it live, no restart — or rejects it with an
error reply while the old value stands. An edit survives any unit restart
(the supervisor holds the value); durable parameter state arrives with
plan/apply in step 5.

`adapters/clock.py` owns civil time: `home/clock/minute` (RFC3339 local
time with offset, on the minute) and `home/clock/date` (at local
midnight), both also published at startup so a late joiner never waits out
a wall-clock minute, and both served from the core cache via get. The
timezone is the clock's own `timezone` manifest parameter — the clock
dogfoods the parameter path.

Try it (needs `uv`):

```
cargo build
PATH="$PWD/target/debug:$PATH" cargo run -- up tests/fixture_house_evening
```

then watch `home/cmd/livingroom/lamp/on` while you flip presence and edit
`off_time` — a zenoh GET on `home/config/evening_lights/off_time` with
payload `"21:30"` changes the running automation's behavior on the next
minute; payload `"03:00"` is rejected (outside `after 20:00, before
02:00`) and changes nothing.

`tests/evening.rs` pins four scenarios against the real supervisor: clock
payloads match the documented schema (asserted through the cache — no
wall-clock waits), presence + a tick crossing `off_time` publishes the
lights-off command, a live `off_time` edit takes effect in the same
process and survives an automation restart via last-value, and an
out-of-constraint write is rejected observably with the old value still
driving behavior. The crossing scenarios run on
`tests/fixture_house_evening_sim/` — the same house without the clock unit
— where the test process publishes `home/clock/minute` itself; the
production clock has no test hooks.

## Recorder / history

`adapters/recorder.py` is the history service — NOT a naive bus mirror.
It subscribes the key spaces its manifest declares and writes a SQLite
store (the path comes from `[discovery]`, `endpoint = "sqlite:<path>"`,
`${VAR}`-expanded recorder-side like adapters' endpoints):

- `home/state/**` and `home/cmd/**` land as **typed samples**: payloads
  are decoded on the way in (`bool` / `number` / `string`; non-scalar or
  non-JSON payloads are dropped with a health event, never a row). Series
  identity is `(class, entity, aspect)`; **room is a tag column**, so an
  entity move is a tag transition on one continuous series, never a new
  series. Timestamps are recorder receive time (µs, UTC), stamped before
  any buffering.
- `home/health/**` and `home/config/**` land raw in an `events` audit
  table — only *accepted* config writes ever reach the bus, so that
  subscription is exactly the accepted-edit audit trail.
- Explicitly not recorded: `home/clock/**`, `home/meta/**`, liveliness.

History reads go **over the bus**, not to the backend: the recorder serves
a queryable at `home/history/**`, keeping the store recorder-private and
swappable (SQLite carries a single home for years; QuestDB is the recorded
growth path). The history key is entity-first — entity is the series
identity, room arrives per row:

```
zenoh get 'home/history/state/lamp/on?from=2026-07-01T00:00:00+02:00;limit=100'
  -> home/history/state/lamp/on
     [{"ts": "2026-07-04T08:26:56.892816+00:00", "room": "livingroom", "value": true}, ...]
```

(`;` separates zenoh selector parameters; `from`/`to` are RFC3339 with
offset, `limit` keeps the most recent rows, replies are ascending, one
reply per concrete series when the key has wildcards.)

If the store becomes unwritable — disk full, permissions, a dying SD card
— samples buffer in memory (bounded, drop-oldest) with their receive-time
timestamps, `{"kind": "backend-outage"}` appears at
`home/health/recorder/event`, and recovery flushes the buffer and reports
`{"kind": "backend-restored", "flushed": N, "dropped": M}` — the outage is
invisible in the data unless the buffer overflowed. A store that cannot
open at startup is a startup error: no readiness, supervisor backoff makes
it visible. Full decisions in
[docs/design.md](docs/design.md#history--recorder-settled-in-step-5a).

Try it (needs `uv`):

```
cargo build
RECORDER_DB=/tmp/history.db PATH="$PWD/target/debug:$PATH" \
  cargo run -- up tests/fixture_house_recorder
```

then publish some `home/state/...` values and get
`home/history/state/{entity}/{aspect}` — the evening-lights loop runs on
the real clock and its state history lands in `/tmp/history.db` and reads
back over the bus.

`tests/recorder.rs` pins four scenarios against the real supervisor
running the step-4 units plus the recorder, asserting on both the store
(rusqlite, dev-dependency) and the bus: state lands with entity, room,
aspect, and correctly typed value (plus the cmd/config/health audit
spaces, and the non-scalar drop); an entity publishing under a new room
continues one series with a tag transition; the backend-outage policy is
observable (store made read-only, samples buffered, health events on
outage and recovery, receive-time timestamps preserved); and the read path
returns what was written, honoring `from`/`limit` and error-replying on a
malformed selector. Each test names its own store via `RECORDER_DB` — no
fixed paths, like no fixed ports, and nothing to install in CI: the store
is embedded.

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
