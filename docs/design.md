# Homeostat: Design Record
 
Status: settled decisions from the founding design discussion (July 2026).
This document is the authority on architecture. Code follows it. Changes to
load-bearing decisions require updating this document first.
 
## Name and framing
 
**Homeostat**, after W. Ross Ashby's 1948 machine. The system maintains a
household in equilibrium. It is not an assistant that waits for commands; it
is a regulator whose setpoints the family adjusts. This framing is the
architectural argument: parameters (setpoints) are family-editable, structure
(the regulating machinery) is owner-governed.
 
## Motivation
 
Replacement for Home Assistant, motivated by:
 
- Text configuration as a first-class citizen. The repo is the single source
  of truth. No hidden state mutated by a UI.
- Pure-code automations, no DSL ceiling.
- Agent-native maintenance: an agent authors and maintains automations
  through the same plan/apply discipline as humans.
- Small core, subtractive design. The runtime is a pure function of
  config + current world state.
Built for the owner's actual device inventory (ESPHome, MQTT, Zigbee2MQTT),
not the general long tail. No Home Assistant bridge in v1.
 
## Architecture overview
 
- **Core (Rust):** config loader, schema validation, template expansion,
  grant-table resolution, plan/apply engine, process supervisor. Owns the
  key space and nothing else of consequence.
- **Bus: Zenoh.** Pub/sub for live state and commands, queryables for reads,
  storage backends for last-known-value. Localhost and remote processes are
  indistinguishable, so machine placement is not an architectural question.
- **Units:** every running thing is a unit: `adapter`, `automation`, or
  `service`. Uniform manifest schema, uniform supervision. Python units are
  uv-run scripts with PEP 723 inline dependencies (one hermetic venv per
  process). Rust units are compiled binaries.
- **Process model:** plain OS processes supervised by the core
  (Erlang/actor-model lineage: fault isolation and language boundaries, not
  microservices). NOT containerized internally. The whole system may run
  inside ONE container as a deployment boundary on a shared host; the core is
  then PID 1 (it must reap orphans and forward signals). Host networking is
  required for mDNS/ESPHome discovery and Zenoh scouting. Config repo mounts
  as a volume.
- **Supervision:** liveliness tokens on the bus, not just PIDs. Restart with
  exponential backoff and a circuit breaker whose state is visible at
  `home/health/{unit}`. (Pattern imported from the Keelson liveliness RFC.)
## Key space
 
```
home/{class}/{room}/{entity}/{aspect}
```
 
- `class`: `state`, `cmd`, `config`, `meta`, `health`, `clock`.
- One room segment, no floor hierarchy in keys.
- Entity names are globally unique (enforced at plan time).
- **Zones never appear in keys.** A zone is a named set of rooms in config.
  Zone subscriptions expand to multiple key expressions at plan time.
- Identity-vs-space: spatial glob `home/state/kitchen/**` means "whatever is
  in this room"; wildcard-room pin `home/state/*/that_lamp/**` means "this
  device wherever it lives". Automations choose explicitly.
- Entity moves are plan/apply migrations: plan lists every key change and
  every subscriber whose match-set changes (dropping to zero matches is a
  warning).
- Non-spatial entities use reserved pseudo-rooms (`global`, `person`),
  validated against a reserved-word list.
- Parameters live on the bus: `home/config/{unit}/{param}` backed by
  last-value storage. Units subscribe to their own config subtree. Parameter
  edits propagate live, no restart.
- Meta: `home/meta/{unit}/manifest_hash`, `home/meta/system/applied_commit`.
## History / recorder
 
The recorder is NOT a naive Zenoh storage mirror. It subscribes to
`home/state/**` and writes to a time-series backend (QuestDB or TimescaleDB)
with entity id as series identity and room as a tag. A move is a tag
transition on a continuous series. Naive Zenoh storage is used only for
last-value on live keys.
 
## Manifest schema
 
TOML. One schema, three kinds. `schema = 1` versioning field at the top of
every manifest and entity file from day one.
 
### Unit manifest (automation example)
 
```toml
schema = 1
 
[unit]
name = "evening_lights"
kind = "automation"          # adapter | automation | service
description = "Dims and turns off downstairs lights at night"
 
[runtime]
command = "uv run units/evening_lights.py"
restart = "on-failure"       # with backoff + circuit breaker, always
shutdown_grace_s = 5
 
[bus.subscribes]
presence = "home/state/downstairs/**/presence"   # zone refs expand at plan time
clock = "home/clock/minute"
 
[bus.publishes]
lights = { key = "home/cmd/downstairs/**/light", capability = "light", priority = "automation" }
 
[params.off_time]
type = "time"
default = "23:00"
constraint = { after = "20:00", before = "02:00" }   # may span midnight
editable_by = "family"
 
[naming]
sv = "kvällsbelysning"
en = "evening lights"
aliases = []
room = "downstairs"          # zone or room, for voice/dashboard grouping
```
 
### Adapter manifest
 
```toml
schema = 1
 
[unit]
name = "zigbee"
kind = "adapter"
 
[runtime]
command = "uv run units/zigbee.py"
restart = "always"
 
[discovery]
mode = "static"              # or "mdns" with service = "..."
endpoint = "mqtt://localhost:1883"   # opaque to core
 
[bus.publishes]
state = { key = "home/state/{room}/{entity}/**" }   # templated, expanded at plan time
 
[bus.subscribes]
commands = "home/cmd/{room}/{entity}/**"
 
[entities]
dir = "entities/zigbee/"     # one file per device
```
 
### Entity file
 
```toml
schema = 1
 
[entity]
id = "0x00158d0003ab1c2d"    # adapter-native address
capability = "light"
features = ["brightness", "color_temp"]
room = "kitchen"             # SINGLE source of spatial truth
 
[naming]
sv = "taklampan i köket"
en = "kitchen ceiling light"
aliases = ["köksbelysningen"]
 
[write_policy]
mode = "shared"              # shared | exclusive | arbitrated
owner = "zigbee"             # exactly one adapter binds each entity
```
 
### Manifest design rules
 
- The entity is the resource; the entity file is the SOLE authority on write
  policy. Automations declare intent (publish expressions), never exclusivity.
  Grants happen at plan time.
- No dependency declarations between units (the bus decouples; dependency
  graphs are rendered from the resolved grant table).
- No version pinning per unit (the repo is the version).
- No health section (derived from liveliness).
- Constraint language stays minimal: min/max, after/before, enum. Anything
  needing more expressiveness means the parameter is `editable_by = "owner"`.
- Templated keys mean the core maintains a derived entity registry. This is
  accepted; it is derived from text, never mutated by a UI. Plan output must
  render the expansion visibly.
- Manifests carry naming/alias/i18n data because they feed voice grammar and
  dashboard generation. Voice quality is a function of manifest hygiene; the
  agent can audit missing aliases.
## Capability and permission model
 
- Plan-time validation resolves every automation's publish expressions
  against the concrete entity set: capability match, write policy, reserved
  classes. Two writers on an `exclusive` entity is a plan error.
- The resolved grant table is part of plan output and doubles as the
  dependency graph.
- Adapters embody entities rather than commanding them; compromising an
  adapter compromises exactly its bound entities, which is irreducible.
- **Arbitrated mode** (from day one): a small arbiter service holds the write
  token per arbitrated entity. Commands carry a priority band; higher
  preempts, preemption events are published. Manual/voice commands occupy the
  top band by convention: THE FAMILY ALWAYS WINS OVER AUTOMATIONS.
  Arbitrated entities' adapters accept commands only via the arbiter's
  output key, giving structural runtime enforcement for high-stakes entities
  (locks, heat pump) without Zenoh ACLs.
- Actor tiers: `owner`, `family`, `automation`, `agent`. Grant changes
  require tier >= owner.
- v1 runtime enforcement is plan-time + trust, except arbitrated entities.
  Zenoh ACLs are the eventual hardening; declarations are already the right
  shape.
- Command payload validity: the SDK's typed command constructors make invalid
  commands unrepresentable in practice; adapters drop invalid payloads with a
  health event. No separate validation layer.
## Plan/apply mechanics
 
- No state file. Desired state is the repo; actual state is queryable from
  the bus (manifest hashes, liveliness, current parameters). Plan diffs repo
  against bus. State drift is impossible by construction.
- **Plan tiers, derived mechanically, never declared:**
  - Parameter-only: config subtree write, no restart. Auto-applicable within
    actor tier. This is the voice path.
  - Behavioral: unit code/manifest changed, grant set unchanged. Restarts
    that unit only.
  - Structural: grant-table delta, unit create/destroy, entity moves,
    write-policy changes. Owner approval required. Plan prints grant-table
    diff, key changes, match-set changes.
  - Any grant-table delta escalates the tier automatically; an agent cannot
    smuggle structural change as a parameter edit.
- **Apply is per-unit and rolling, not transactional.** Adapters before
  dependent automations. Per unit: write config, restart if needed, await
  liveliness + healthy heartbeat, proceed. Failure halts the walk in place
  and reports position. No automatic whole-plan rollback.
- **Rollback is git.** Applied plans record the commit hash. Rollback =
  plan against the previous commit = a normal forward plan.
- **Pending plans are files** (`plans/pending/{id}.plan`): diff, grant delta,
  actor, timestamp, base commit. Survive restarts, mobile-reviewable.
  Auto-invalidate if the repo moves past their base commit.
- One apply at a time (core holds the lock). Parameter fast-path writes are
  exempt. Voice-initiated changes commit with the transcript as the message.
## Agent surface (MCP)
 
Tools: `read_state`, `read_history`, `propose`, `plan`, `apply`. The agent
never touches the bus directly for structural work; it manipulates text and
goes through plan/apply like every other actor. Agent-authored parameter
edits within constraints auto-apply; structural changes land as pending
plans for owner approval.
 
## Voice (later phase)
 
- Two-tier command path: a fast-path intent matcher (high precision,
  deliberately narrow, no fuzzy guessing; ambiguity falls through to the
  agent) and the conversational agent as fallback.
- The fast-path grammar is GENERATED from manifests + key-space schema at
  plan/apply time, as a build artifact of the same transaction. Never
  hand-maintained. Stale grammar is impossible by construction.
- Grammar generation runs house-side only; the public tool never sees
  private naming data.
- ESPHome voice satellites; local wake word + STT; no cloud in the fast path.
- Agent sessions: short-lived, satellite-scoped, expire after ~1 min silence.
## Repo split
 
- **Public (`homeostat`):** Rust core, schema definitions (versioned),
  Python SDK (typed commands, config helpers, automation Context), generic
  adapters (Zigbee2MQTT, ESPHome, clock, arbiter, recorder), generic agent
  skills, an example house as documentation.
- **Private (house repo):** all manifests, entity files, zones, automations,
  house-specific agent skills, pending plans, applied-commit metadata. Pins
  a core version; CI runs `homeostat plan --check` on push.
- Boundary test: device address, family name, room name, or behavioral
  choice => private. Identical in a stranger's house => public.
- Generic automations graduate from private to public SDK helpers/examples.
- Invariant: the public tool never sees the private repo except locally.
## Name collision status (checked 2026-07-03)
 
crates.io: free. PyPI: free. npm: free. Homebrew: free. GitHub username and
Docker Hub namespace `homeostat` are squatted but empty; publish under
`freol35241/homeostat` and `ghcr.io/freol35241/homeostat`. `homeostat.dev`
is parked; `.io`/`.org` unregistered. No trademark risk surfaced (generic
1948 scientific term).
 
## Build sequence
 
1. **Key space + manifest parser + validator, no runtime.** CLI reading a
   repo of manifests and entity files: template expansion, zone expansion,
   grant-table resolution, `homeostat plan` against an empty world. Pure
   Rust, serde types, test corpus of manifest files. THIS IS THE CURRENT
   PHASE.
2. Supervisor + one trivial (fake) adapter: process spawning, liveliness,
   restart with backoff, meta key space.
3. First real adapter: Zigbee2MQTT (translating subscriber).
4. First automation + live parameter path end to end.
5. Recorder, then plan/apply proper, then agent MCP surface, then voice.
Risk lives in steps 1 and 2; everything after is accretion.
 
## Open questions (flagged, not settled)
 
- Civil time semantics: the clock service owns timezone/DST (Sweden has real
  transitions); constraints evaluate against it, never naive comparison.
  Exact clock-service key schema TBD in step 2.
- Whether `features` should gate command contents beyond SDK constructors.
  Current lean: no separate layer.
- Zenoh ACL hardening timeline.
- Dashboard: generated from manifests (parameters + entities), design TBD
  after step 4.