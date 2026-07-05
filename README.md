# Homeostat

**A household regulator, not an assistant.** Text-first home automation:
your home's entire configuration lives in a git repo, a small Rust core
supervises plain OS processes over a [Zenoh](https://zenoh.io/) bus, and
every change — a family member nudging a setpoint, you rewriting an
automation, an AI agent proposing one — goes through the same
`plan` / `apply` discipline.

Named after W. Ross Ashby's 1948 machine. The system maintains the home in
equilibrium; the family adjusts setpoints; the owner governs structure.
[docs/design.md](docs/design.md) is the full design record and the
authority on architecture.

## Why

Homeostat is built as a Home Assistant replacement around a few firm
opinions:

- **The repo is the single source of truth.** No hidden state mutated by a
  UI. Desired state is text in git; actual state is read live from the
  bus; there is no state file, so drift is impossible by construction.
- **Automations are pure code.** Python scripts with a small SDK — no YAML
  DSL ceiling. A unit gets exactly the bus surface its manifest declares,
  nothing more.
- **Changes are reviewed, not clicked.** `homeostat plan` shows exactly
  what would change and derives how disruptive it is — parameter-only,
  behavioral, or structural — mechanically from the diff, never from a
  declaration. `homeostat apply` walks the difference unit by unit,
  rolling, halting visibly on failure. Rollback is `git checkout` and
  apply again.
- **Agent-native.** An MCP server lets an agent read state and history,
  propose edits, and apply them — with authority bounded by the same
  plan/apply machinery as every other actor. A setpoint change
  auto-applies; anything structural waits for the owner.
- **Small core, real processes.** Every running thing is a supervised OS
  process with liveliness tokens, exponential restart backoff, and a
  circuit breaker visible on the bus — Erlang lineage, not containers or
  plugins.

It is early software (see [Status](#status)) and currently targets its
author's device inventory: Zigbee2MQTT, ESPHome, MQTT. There is no Home
Assistant bridge.

## How it works

A **house repo** declares everything about your home:

```
house/
  zones.toml              # named sets of rooms; zones never appear in keys
  units/
    zigbee.toml           # adapter: discovery endpoint, templated keys, entities dir
    evening_lights.toml   # automation: subscribes, publishes, params
    recorder.toml         # service
  entities/
    zigbee/
      kitchen_ceiling.toml   # one file per device; file stem = entity name
```

The entity file is the sole authority on write policy (`shared`,
`exclusive`, `arbitrated`); its `room` field is the single source of
spatial truth. `examples/house/` is a complete, documented example.

`homeostat up` validates the repo, then runs every unit as a supervised
process, all talking over a well-defined key space on the bus:

```
home/state/{room}/{entity}/{aspect}   live device state
home/cmd/{room}/{entity}/{aspect}     commands toward devices
home/config/{unit}/{param}            live-editable setpoints
home/health/{unit}                    supervision status, health events
home/history/**                       recorded history, served over the bus
```

Authority is tiered along the same lines as the plan tiers: parameters are
family-editable live on the bus (validated against manifest constraints);
structure — units, grants, code — changes only through the repo and
plan/apply.

## Quick start

You need a Rust toolchain, and [uv](https://docs.astral.sh/uv/) to run the
Python units.

**Plan a house** (offline, against the empty world):

```
git clone https://github.com/freol35241/homeostat && cd homeostat
cargo run -- plan examples/house
```

On a valid repo this prints every unit to create, the expanded key
space, and the resolved grant table; on an invalid repo, the complete
error list and a non-zero exit — a house repo's CI in one command:

```
Homeostat plan
  repo:  examples/house
  world: empty

Units to create (4):

+ adapter zigbee (units/zigbee.toml)
    ...
+ automation evening_lights (units/evening_lights.toml)
    params:
      off_time  type=time  default=23:00  constraint={after=20:00, before=02:00}  editable_by=family

Grant table:

  evening_lights.lights  capability=light  priority=automation
    -> kitchen_ceiling  (room=kitchen, write=shared, owner=zigbee)
    ...

Plan tier: structural (4 units created, 1 grant added)
```

**Run a live house.** The integration-test fixtures double as demos; this
one runs a clock, a reflector adapter (echoes commands back as state), and
the `evening_lights` automation:

```
cargo build
PATH="$PWD/target/debug:$PATH" cargo run -- up tests/fixture_house_evening
```

(The `PATH` prefix is only for fixtures, whose adapters are test binaries
built by this crate; real houses use commands that resolve on their own,
like `uv run units/...`.)

The supervisor opens a router-mode Zenoh session on `tcp/127.0.0.1:7447`
(override with `--listen`), spawns each unit, and logs health transitions
(`starting` → `running`). Any Zenoh client can now watch
`home/state/**`, publish commands, or edit a setpoint live: a zenoh GET on
`home/config/evening_lights/off_time` with payload `"21:30"` changes the
running automation on the next minute — and payload `"03:00"` is rejected
against the manifest constraint with the old value still in force.

**Make a change through plan/apply.** With a house running, edit the repo
and let the engine work out what it means:

```
PATH="$PWD/target/debug:$PATH" target/debug/homeostat up tests/fixture_house_apply &
target/debug/homeostat plan tests/fixture_house_apply --bus tcp/127.0.0.1:7447
# -> No changes. The world matches the repo.
sed -i 's/default = 1/default = 5/' tests/fixture_house_apply/units/probe.toml
target/debug/homeostat apply tests/fixture_house_apply --bus tcp/127.0.0.1:7447
# -> Plan tier: parameter-only (1 parameter change) ... Applied.
```

The running unit picks up the new value with zero restarts. Edit its
`probe.py` instead and the same command plans behavioral and restarts
exactly that unit.

**Run with Docker.** Each release publishes a container image for
linux/amd64 and linux/arm64 alongside prebuilt binaries. The image
carries everything a deployed house needs — the `homeostat` binary, git,
uv, and a pre-installed Python:

```
docker run -d \
  -v /path/to/house:/house \
  -v homeostat-uv:/var/cache/uv \
  -p 7447:7447 \
  ghcr.io/freol35241/homeostat
```

The default command is `up /house --listen tcp/0.0.0.0:7447`, so the bus
is reachable through the published port. The uv cache volume is optional
but keeps unit environments across container replacements.
[`examples/starter-house`](examples/starter-house/) is a runnable
template for that mounted house — clock, recorder, Zigbee2MQTT adapter,
and the evening-lights automation, with a compose file for the full
mosquitto + zigbee2mqtt + homeostat stack; copy it out and make it your
own repo. The other subcommands work through the same image:

```
docker run --rm -v /path/to/house:/house ghcr.io/freol35241/homeostat \
  plan /house --bus tcp/<supervisor-host>:7447
```

## The pieces

### Adapters: devices onto the bus

`adapters/zigbee2mqtt.py` is the reference adapter — a translating
subscriber that fans Zigbee2MQTT state out to per-aspect keys and
translates commands back:

```
zigbee2mqtt/lamp_kitchen_1  {"state":"ON","brightness":128}
  -> home/state/kitchen/kitchen_lamp/on          true
  -> home/state/kitchen/kitchen_lamp/brightness  128

home/cmd/kitchen/kitchen_lamp/on  true
  -> zigbee2mqtt/lamp_kitchen_1/set  {"state":"ON"}
```

Unknown devices and malformed payloads are dropped with a health event at
`home/health/{unit}/event`, never a crash. Details and conventions:
[design record §Zigbee2MQTT](docs/design.md#zigbee2mqtt-adapter-and-python-sdk-settled-in-step-3).

Adapters that can enumerate their periphery also publish a discovery
document at `home/discovery/{unit}` — every paired device with its
binding id, whether an entity file claims it yet, and a suggested
capability stanza — which is how an agent constructs entity files for
unconfigured devices ([design record §Discovery](docs/design.md#discovery-settled-2026-07-05)).

### Automations: regulators, not schedulers

Automations are Python scripts built on the SDK in `sdk/python/`. A unit
declares it in its PEP 723 header, pinned to a release tag:

```python
# /// script
# requires-python = ">=3.11"
# dependencies = ["homeostat"]
#
# [tool.uv.sources]
# homeostat = { git = "https://github.com/freol35241/homeostat", subdirectory = "sdk/python", tag = "v0.1.0" }
# ///
```

Because the pin lives in the script itself — a file plan/apply hashes —
an SDK upgrade is a visible behavioral change: `plan` flags it, `apply`
restarts exactly the units that bumped. (Units inside this repo use a
relative `path` source instead, so tests exercise the working-tree SDK.)

The automation context gives a unit exactly the surface its manifest declares:

```python
from homeostat import automation

ctx = automation.context()              # reads units/{HOMEOSTAT_UNIT}.toml
ctx.subscribe("presence", on_presence)  # binding names from [bus.subscribes]
ctx.subscribe("clock", on_clock)
ctx.params.off_time                     # typed (datetime.time), live-updated
ctx.publish("lights", False, room="livingroom", entity="lamp")
ctx.ready(); ctx.run()                  # liveliness token, block until SIGTERM
```

Parameters live at `home/config/{unit}/{param}`, seeded from manifest
defaults, validated against constraints (min/max, after/before with
midnight spanning, enum) on every write, and delivered to running units
live — no restart. Committing a new default to the repo is what makes a
live edit durable. A `clock` service owns civil time at
`home/clock/minute` and `home/clock/date`.

### The recorder: history as a service

`adapters/recorder.py` writes state and commands to SQLite as typed
samples — entity is the series identity, room is a tag, so moving a device
between rooms continues one series — plus an audit trail of health events
and accepted config edits. Reads go over the bus, keeping the store
private and swappable:

```
zenoh get 'home/history/state/lamp/on?from=2026-07-01T00:00:00+02:00;limit=100'
```

A backend outage (full disk, dying SD card) buffers samples in memory with
their original timestamps and reports itself as health events; recovery
flushes the buffer. Details:
[design record §History](docs/design.md#history--recorder-settled-in-step-5a).

### Plan / apply

`homeostat plan --bus <endpoint>` reads the live world through the core's
queryables and diffs it against the repo. The tier is derived, never
declared:

- **parameter-only** — setpoint differences; applies with zero restarts.
- **behavioral** — a unit's manifest or code changed; restarts exactly
  that unit.
- **structural** — units created/destroyed or any grant-table delta; the
  plan renders the grant diff.

`homeostat apply` commands the running supervisor to walk the difference —
per-unit, rolling, in grant order, awaiting health after each — and halts
in place on failure with the position printed; re-running plans exactly
the remaining work. `plan --save` writes a pending plan file, reviewable
on a phone, that auto-invalidates when the repo moves.

### The agent surface: MCP

`homeostat mcp` serves five tools — `read_state`, `read_history`, `plan`,
`propose`, `apply` — over stdio, or over HTTP as a supervised service unit
in a deployed house. The agent never touches the bus directly for
structural work: `propose` takes file contents, commits, and plans. A
parameter-only plan auto-applies (the commit *is* the edit); anything
behavioral or structural is saved as a pending plan for the owner to apply
with `homeostat apply --plan <file>`. The tier derivation is the
enforcement — a manifest edit that smuggles in a grant delta escalates to
structural on its own. Details:
[design record §Agent surface](docs/design.md#agent-surface-mcp).

## Status

Pre-1.0, under active development, following the build sequence in the
design record:

1. ✅ Key space, manifest parser, validator, `homeostat plan`
2. ✅ Process supervisor: liveliness, backoff, circuit breaker
3. ✅ Zigbee2MQTT adapter and Python SDK
4. ✅ First automation, clock service, live parameter path
5. ✅ Recorder / history, then plan/apply proper
6. ✅ Agent MCP surface
7. ⬜ Voice

## Development

```
cargo test
```

Integration tests run the real binary against real infrastructure — a live
supervisor, a real mosquitto broker on a free port, a real SQLite store —
never mocks. The invalid-manifest corpus in `tests/corpus/invalid/` pairs
each broken house with its complete expected error list. CI needs
`mosquitto` and `uv` installed. CI also builds the container image and
runs `scripts/smoke_image.sh` against it — a packaging test that boots a
minimal house in the image and asserts a unit reaches `running`, the bus
answers a second container, and SIGTERM shuts down cleanly.

| Test | Pins |
| --- | --- |
| `tests/supervision.rs` | the unit contract: spawn, crash/backoff, breaker, clean shutdown |
| `tests/z2m.rs` | adapter translation both ways, drop policy, unit contract |
| `tests/evening.rs` | automation behavior, live parameter edits, constraint rejection |
| `tests/recorder.rs` | typed history, room-tag transitions, outage buffering, bus reads |
| `tests/plan_apply.rs` | tier derivation, rolling apply, halt-in-place, stale plans |
| `tests/mcp.rs` | agent reads, propose/auto-apply, tier gating, grant-smuggling escalation |

## License

[Apache-2.0](LICENSE)
