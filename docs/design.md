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
## Supervision (settled in step 2)

### The unit contract

What the supervisor guarantees to every unit, and what every unit owes back.

Supervisor -> unit, at spawn:

- `runtime.command` is whitespace-tokenized and exec'd directly — no shell,
  so no quoting in v1 manifests. Lookup uses PATH; relative paths resolve
  against the house repo root, which is the unit's cwd.
- Each unit runs in its own process group. On Linux the child additionally
  gets `PR_SET_PDEATHSIG(SIGKILL)`, so even a SIGKILLed supervisor cannot
  leak orphans. If the direct child exits on its own, the supervisor sweeps
  the remainder of its process group before applying the restart policy —
  a unit's descendants (e.g. the interpreter under a `uv run` wrapper)
  never outlive its leader, so a survivor can't keep the liveliness token
  alive and poison the next incarnation.
- Environment: `HOMEOSTAT_UNIT` (the unit's name) and `HOMEOSTAT_BUS` (the
  Zenoh endpoint to connect to, e.g. `tcp/127.0.0.1:7447`).

Unit -> bus, obligations:

- Connect to `HOMEOSTAT_BUS` as a Zenoh client. The supervisor's session
  runs in router mode — the hub that routes between units and observers
  (Zenoh peers do not route between clients, and peer linkstate routing was
  removed in Zenoh 1.9). Multicast scouting stays off; topology is explicit.
- Declare a liveliness token at `home/health/{unit}/alive` once actually
  ready. The token — not the PID — is what "up" means; the supervisor only
  reports `running` after the token appears.
- On SIGTERM, exit cleanly within `shutdown_grace_s` (default 5s). After the
  grace the whole process group gets SIGKILL.

### Health key schema

The supervisor publishes JSON at `home/health/{unit}` on every transition;
since step 4 the core's last-value cache serves the current value to late
joiners via a queryable (this replaced a 1s-republish stopgap):

```json
{
  "status": "starting | running | backoff | open | stopped",
  "pid": 1234,
  "restarts": 2,
  "backoff_ms": 400,
  "last_exit_code": 1
}
```

- `starting`: process spawned, token not yet seen. `running`: token present.
- `backoff`: process exited, restart scheduled in `backoff_ms` (present only
  in this status).
- `open`: circuit breaker open, no further restarts until the supervisor is
  restarted.
- `stopped`: not coming back — policy `never`, clean exit under
  `on-failure`, or supervisor shutdown.

Restart policy per manifest (`always` / `on-failure` / `never`). Backoff is
exponential: 100ms base, doubling, capped at 30s. A run that survives 5s
resets the consecutive-failure counter; the 5th consecutive quick exit opens
the breaker. Any quick exit counts — a clean-exit loop is as much a crash
loop as a panic loop.

The supervisor also publishes `home/meta/{unit}/manifest_hash` (sha256 hex
of the manifest file) at startup.

### Clock key schema

Documented in step 2, implemented in step 4 (see the step-4 section):

- `home/clock/minute` — published each minute on the minute; payload is
  RFC3339 local time with offset, e.g. `2026-07-03T21:04:00+02:00`.
- `home/clock/date` — published at local midnight; payload `2026-07-03`.
- The clock service owns timezone and DST; subscribers never do naive time
  arithmetic.

## Zigbee2MQTT adapter and Python SDK (settled in step 3)

### How an adapter learns its configuration

An adapter reads its own manifest at `units/{HOMEOSTAT_UNIT}.toml` and the
entity files in its `[entities].dir` — the same files the core already
validated; cwd is the house root, so paths are relative. There is no second
config channel and no core-to-adapter config protocol.

The discovery endpoint may reference environment variables (`${VAR}`),
expanded by the adapter — endpoints are opaque to the core, and ports or
credentials don't belong in the repo. An unset variable is a startup error
(the supervisor's backoff makes it visible).

Entity binding for z2m: the entity file's `id` is the Zigbee2MQTT topic
segment (`zigbee2mqtt/{id}` — the friendly name or IEEE address), the file
stem is the bus entity name, `room` comes from the entity file. The base
topic `zigbee2mqtt` is a constant for now; the adapter subscribes
`zigbee2mqtt/+`, which keeps `bridge/#` traffic out and means friendly
names containing `/` are unsupported.

### Bus payload conventions

Payloads on `state` and `cmd` keys are bare JSON values.

State: a z2m JSON object fans out per top-level field to
`home/state/{room}/{entity}/{field}`. The z2m `state` field is normalized —
adapter-native vocabulary does not leak onto the bus:

- lights/switches: aspect `on`, boolean (`"ON"` → `true`)
- locks: aspect `locked`, boolean (`"LOCKED"` → `true`)

Other scalar fields pass through under their z2m names (`brightness`,
`temperature`, `occupancy`, ...). Composite fields (objects/arrays, e.g.
`color`) are deferred.

Commands: the payload on `home/cmd/{room}/{entity}/{aspect}` is one JSON
value. `on` + boolean translates to `{"state": "ON"|"OFF"}`; any other
aspect passes through as `{aspect: value}` to `zigbee2mqtt/{id}/set`.

Locks are state-only until the arbiter exists: the adapter declares no
command subscription at all for lock entities — not subscribing IS the
structural enforcement for arbitrated entities in the meantime.

Dropped input never crashes the adapter and always leaves a trace: a JSON
event at `home/health/{unit}/event`, e.g.
`{"kind": "drop", "reason": "unknown-device", "topic": "zigbee2mqtt/x"}`
(reasons so far: `unknown-device`, `malformed-payload`, `invalid-command`).
The parent key `home/health/{unit}` remains supervisor-owned.

### Python SDK

Lives at `sdk/python/`, package name `homeostat`. Minimal bootstrap, grown
by need:

- `homeostat.session` — `connect()` reads `HOMEOSTAT_UNIT`/`HOMEOSTAT_BUS`
  and opens a client session (scouting off); `UnitSession.ready()` declares
  the liveliness token — call it only once the unit can actually do its
  job; `put_json` / `subscribe` / `health_event` / `close`.
- `homeostat.keys` — key builders mirroring the Rust `src/bus.rs`.
- `homeostat.house` — adapter-side manifest and entity loading.

Python units consume it via PEP 723 inline metadata with a `[tool.uv.sources]`
path source (`homeostat = { path = "../sdk/python" }`, resolved relative to
the script file regardless of cwd); PyPI publication comes later. `uv sync
--script <unit>.py` pre-warms a unit's environment so first-run dependency
resolution never eats into supervision timeouts (CI does this before
`cargo test`).

## First automation and the live parameter path (settled in step 4)

### Last-value lives in the core, not a storage plugin

The core owns an in-memory last-value cache inside the supervisor process,
served over the bus by queryables. It backs three key spaces:

- `home/config/{unit}/{param}` — the parameter path (below).
- `home/health/{unit}` — replaces the step-2 1s-republish stopgap. The
  supervisor publishes health only on transitions; a queryable serves the
  current value to late joiners.
- `home/clock/*` — the core mirrors clock publications so a late joiner
  (or a test) can `get` the current minute/date instead of waiting out a
  wall-clock minute.

Why not the Zenoh storage plugin: it is a heavy, version-coupled dependency,
and a passive mirror cannot reject an out-of-constraint write — validation
needs to sit on the write path anyway, so the write path and the cache
belong to the same owner. The read pattern everywhere is *subscribe, then
get, merge*: the subscriber catches everything after the get; the get covers
everything before it.

The cache is in-memory: parameter edits survive any unit restart (the
supervisor holds the value) but not a supervisor restart — defaults re-seed
from manifests. Durable parameter state arrives with plan/apply (step 5),
where a parameter edit is a repo commit; the bus cache is a live view, not
the system of record.

### The parameter write path

Only the core ever puts on `home/config/**`. It seeds each unit's parameters
from manifest defaults at startup and declares a queryable on
`home/config/*/*`:

- **GET without payload** — read: replies the current JSON value.
- **GET with payload** — write request: the core validates the JSON payload
  against the manifest's type and constraint (`min`/`max`, `after`/`before`
  with midnight spanning, `enum`). Accepted: the value is stored, put on the
  key (every subscribed unit sees it live, no restart), and echoed in an ok
  reply. Rejected: the query gets an **error reply** naming the violation —
  synchronously observable to the writer — no put happens, and the old value
  stands.

Units never subscribe to config in their manifests; subscribing to your own
`home/config/{unit}/*` subtree is implicit and the SDK does it for you.
Actor-tier enforcement of `editable_by` waits for plan/apply and Zenoh ACLs;
v1 is plan-time + trust, as everywhere else.

### SDK automation Context

`homeostat.automation.context()` reads the unit's own manifest (same file
the core validated) and gives an automation exactly its declared surface:

- `ctx.subscribe(binding, handler)` — binding names from `[bus.subscribes]`;
  the handler gets `(key, value)` with the JSON payload decoded.
- `ctx.params.name` — typed current values (`time` → `datetime.time`),
  seeded via get and updated live by a config subscription.
- `ctx.publish(binding, value, room=..., entity=..., aspect=...)` — publish
  expressions from `[bus.publishes]`. Publishes go to **concrete keys only**
  (a put on a `**` expression would hand adapters an unparseable wildcard
  key); literal segments of the expression are defaults, wildcard segments
  must be named, and the SDK refuses any key the declared expression does
  not cover — the manifest stays the authority on intent.
- `ctx.ready()` / `ctx.run()` — liveliness token, then block until SIGTERM.

### Clock service

A Python service (`adapters/clock.py`, generic and public) on the SDK's
Context, stdlib zoneinfo for real DST handling. Timezone comes from its own
manifest: `[params.timezone]`, type `string`, `editable_by = "owner"` — the
clock dogfoods the live parameter path. Payloads are bare JSON strings like
all bus payloads: `"2026-07-03T21:04:00+02:00"` on `home/clock/minute`,
`"2026-07-03"` on `home/clock/date`.

The clock publishes the *current* minute and date immediately at startup
before declaring ready, then on each boundary. That startup publish is
late-joiner catch-up, not a test hook — a restarted subscriber must not run
blind for up to 59 seconds. Tests exploit it plus the core clock cache to
assert the schema without waiting; the off-time-crossing scenarios run on a
fixture house with no clock unit at all, where the test process publishes
`home/clock/minute` itself. Nothing in any production path knows tests
exist; an automation cannot tell who publishes clock keys.

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
## History / recorder (settled in step 5a)
 
The recorder is NOT a naive Zenoh storage mirror. It subscribes to
`home/state/**` and writes a time-series store with entity id as series
identity and room as a tag. A move is a tag transition on a continuous
series. Naive Zenoh storage is used only for last-value on live keys.
Payloads are decoded and typed on the way in; anything that fails to decode
leaves a health event, never a row of garbage.

### Backend: SQLite, embedded in the recorder — production AND tests

The founding candidates were QuestDB or TimescaleDB. v1 uses neither: a
single home produces well under ten samples a second, and a SQLite file
with a series index absorbs years of that without noticing. The heavier
engines cost what this system refuses to pay: a permanent JVM (or a
Postgres cluster) on the home server, a provisioning/supervision story the
core doesn't have (the backend is not a unit), and CI setup beyond
`cargo test` on a stock runner. Choosing a server backend for production
and an embedded one for tests would hollow the tests out — so there is no
dual path: the identical engine runs in both.

What makes the backend swappable later is the read path: history reads go
over the bus (below), so the store is recorder-private. Outgrowing SQLite
means a behavioral change to one unit, not a structural change to the
system. QuestDB remains the designated growth path if volume or analytical
queries ever demand it. DuckDB was considered and rejected as the store —
the recorder's workload is high-frequency tiny appends plus small indexed
range reads (OLTP-shaped, SQLite's grain), while DuckDB is a columnar OLAP
engine that is weak at frequent single-row inserts and single-process by
design (no other process can read the file while the recorder writes; the
tests and any live backup/inspection depend on exactly that). But it
composes: DuckDB's `sqlite` extension can ATTACH the store file read-only,
so an analytical layer (downsampling, long-range aggregation) can sit on
top of the same SQLite file later — additive, no recorder change, no
migration.

The store location comes from the recorder's `[discovery]` section
(`endpoint = "sqlite:<path>"`, path relative to the house root, `${VAR}`
expansion recorder-side like adapters). `[discovery]` is therefore legal on
services as well as adapters — required for adapters, optional for
services, still an error on automations.

### Schema

Two tables. Scalar samples from room/entity/aspect keys:

```sql
samples(ts, class, room, entity, aspect, kind, value)
  -- ts:     µs since epoch, UTC, recorder receive time
  -- class:  'state' | 'cmd'
  -- kind:   'bool' | 'number' | 'string'; value stored natively per kind
  -- index:  (class, entity, aspect, ts)
```

Series identity is `(class, entity, aspect)`; `room` is a tag column. An
entity move is consecutive rows whose tag changes — one continuous series,
never a new one.

The timestamp is recorder receive time, not the zenoh sample timestamp:
sample timestamps are optional (client sessions don't stamp by default),
and one consistent clock source beats mixed provenance. On a single-host
bus the skew is microseconds. Timestamps are assigned at receive, before
any buffering, so a backend outage never distorts history.

Non-scalar payloads (JSON objects, arrays, null) are not recorded:
composite fields are deferred by design (step 3), so their appearance is a
bug worth a trace — a `drop` health event at `home/health/recorder/event`
— not data. Non-JSON payloads likewise.

Audit events from unit/param keys, raw JSON, no typing:

```sql
events(ts, key, payload)   -- index: (key, ts)
```

### Recorded key spaces

- `home/state/**` → samples, class `state`.
- `home/cmd/**` → samples, class `cmd` — what was commanded, when. "Who"
  arrives when command payloads carry actors (arbiter phase).
- `home/health/**` → events: supervisor transitions and unit drop events.
  (Liveliness tokens are not samples and don't appear.)
- `home/config/**` → events: only *accepted* writes ever land on config
  keys (rejects never put), so this subscription IS the accepted-edit
  audit trail. The step-4 rule "units never subscribe to config in their
  manifests" is about consuming your own parameters (the SDK does that
  implicitly); the recorder subscribes `home/config/**` as data, declared
  in its manifest like any other subscription.

Explicitly NOT recorded: `home/clock/**` (a derivable row per minute,
forever — history queries don't need it), `home/meta/**`, liveliness
tokens, and `home/history/**` itself.

A recorder restart is a gap in history: there is no bus replay in v1; the
supervisor's `always` restart policy keeps the gap small.

### Read path: over the bus

`home/history` is a key class. The recorder declares a queryable at
`home/history/**`; a GET on

```
home/history/{state|cmd}/{entity}/{aspect}?from=<RFC3339>;to=<RFC3339>;limit=<n>
```

(`;` is zenoh's selector-parameter separator; RFC3339 offsets contain `+`
and `&` would need escaping zenoh doesn't do.)

returns one reply per concrete series (reply key = concrete history key),
payload a JSON array of `{"ts": <RFC3339 UTC>, "room": ..., "value": ...}`
ascending; `limit` (default 1000) keeps the most recent rows in range.
Wildcards in the entity/aspect slots fan out to one reply per matching
series. A malformed selector gets an error reply.

The history key is entity-first — no room slot — because entity is the
series identity and room is a tag carried per row: a moved entity is ONE
key whose rows show the tag transition. Reads over the bus keep the
backend recorder-private (the step-6 agent needs zero backend knowledge or
credentials) and give history the same access story as everything else
(future Zenoh ACLs). The recorder declares the queryable surface under
`[bus.publishes]` — replies are data the unit originates, and the plan
renders the read surface visibly.

### The recorder unit

Python on the SDK (`adapters/recorder.py`, generic and public like the
clock); `sqlite3` is stdlib, so no new dependencies. Subscriber callbacks
stamp, type, and enqueue; a single writer thread drains the queue, one
transaction per flush, on a connection opened per flush — the failure
domain is "can I open and commit right now", with no long-lived handle to
hold stale permissions or a deleted inode. Reads open their own read-only
connections (readers and the writer never share a handle).

### Failure policy: bounded buffer + flush

- Startup: the store must open and its schema initialize before `ready()`
  — a recorder that never had a working store must not claim readiness.
  Failure (including an unset `${VAR}`) is a startup error, visible
  through the supervisor's backoff.
- Runtime: a failed flush keeps the batch queued (bounded, 10,000 samples,
  drop-oldest — recent state is worth more than old) and emits
  `{"kind": "backend-outage", ...}` at `home/health/recorder/event` once
  per down-transition, not per retry. Retries happen on new samples and on
  a ~1s timer.
- Recovery: the buffer flushes and `{"kind": "backend-restored",
  "flushed": N, "dropped": M}` is published. Buffered samples land with
  their receive-time timestamps — the outage is invisible in the data
  unless the buffer overflowed.
- Reads during a write outage are attempted normally and usually still
  work (disk-full and permission failures don't stop reading); a read
  error becomes an error reply.

For an embedded backend, "the backend is down" means the store file became
unwritable — disk full, permissions, dying SD card. That is what the
integration test induces (chmod the store read-only, publish, restore) and
what the policy above is written against; no production code path knows
tests exist.
 
## Plan/apply proper (settled in step 5b)

The founding mechanics (below, "Plan/apply mechanics") stand; this section
records how they became concrete. Durable parameter state arrives here: the
repo is the system of record, the bus cache is a live view.

### How plan sees the live world

`homeostat plan [path] --bus <endpoint>` (falling back to `HOMEOSTAT_BUS`)
connects to the supervisor's bus as a client and reads the world through
the core's existing last-value queryables — no second channel:

- `home/meta/{unit}/manifest` (raw TOML as loaded), `.../manifest_hash`,
  `.../files_hash`, `home/meta/system/grants`,
  `home/meta/system/applied_commit` — a new meta cache/queryable in the
  core, same pattern as config/health/clock (step 4). Startup previously
  only *put* manifest hashes; late joiners could never read them.
- `home/health/*` — unit status.
- `home/config/*/*` — current parameter values.

With no endpoint anywhere, plan runs offline against the empty world,
labeled as such — still what a house repo's CI wants. An endpoint that is
given but unreachable is a hard error, never a silent empty world: a plan
that says "create everything" against a house that is merely unreachable
is how you double-start a home.

### What "changed" means: two hashes, then semantics

- `manifest_hash` — sha256 of the manifest file (as in step 2).
- `files_hash` — sha256 over the unit's non-manifest repo inputs: command
  tokens that resolve to files under the house root (`uv run
  units/foo.py` hashes the script), an adapter's entity files, and
  `zones.toml` when any of the unit's key expressions referenced a zone.

Hash-equal units are unchanged. A manifest-hash mismatch is classified
semantically: both manifests are parsed and compared with every param's
`default`/`constraint`/`editable_by` stripped. Equal after stripping (and
files unchanged, and no grant delta) → the change is parameter-level →
parameter-only tier. Anything else — including param add/remove or type
change, since a running unit read its manifest at startup — is behavioral.
Grant-table delta or unit create/destroy escalates to structural, as
always. Parameter diffs themselves come from comparing live values against
repo defaults, which covers both a changed default and live drift with one
rule.

### Who executes apply

The CLI commands the running supervisor over the bus: a core-owned control
queryable at `home/meta/system/apply`, GET-with-payload = apply request
(the same query-as-command pattern as config writes). The supervisor
executes the walk itself — it owns the process table, the per-unit
backoff/breaker state, and the health map, so restart-and-await-readiness
composes with supervision instead of racing it. The alternatives lose:
a CLI-side walk needs remote per-unit stop/start controls plus its own
lock anyway, and signal-and-re-read gives no plan verification and no
result channel.

The supervisor holds the apply lock (one apply at a time); parameter-only
applies bypass it. On request it re-reads and re-validates the repo from
disk and derives its own diff against its in-memory world — the CLI's
printed plan is a preview; the supervisor's diff is what executes. "Await
liveliness + healthy heartbeat" is defined as: health `running`, which by
construction means the liveliness token is present. A deliberate apply
restart gets a fresh supervise task and therefore a fresh breaker — new
code earns a fresh failure budget, and a unit stuck in backoff/open can be
replaced mid-cycle.

### The walk

Derived from the grant table (automation → granted entities → owner
adapter ⇒ adapter before dependent automation), never declared:

1. Parameter writes (no restarts; a unit about to restart just reads the
   new value on start).
2. Removals, in reverse grant order — dependents stop before the adapters
   they write through.
3. Creates and restarts, in grant order; after each unit: await health
   `running`, halt on breaker `open`, `stopped`, or a readiness deadline.

Grant-edgeless units and ties order by kind (adapter, automation,
service), then name — deterministic. Failure halts the walk in place:
exit code 1, the CLI prints the halt position (applied / halted-at /
not-reached), the apply reply carries per-step results, the failed unit's
state is visible at `home/health/{unit}`, earlier units keep running
their new incarnations, later units are untouched, and neither
`applied_commit` nor `home/meta/system/grants` advances — a re-run plans
exactly the remaining work.

### Parameter drift

Plan renders every live≠repo parameter (`~ evening_lights/off_time
live="21:30"  repo="23:00"`); drift is always visible. Apply sets live = repo:
the repo is the system of record, and a live edit the family wants to
keep is made durable by committing it (edit the manifest default; that
plan is parameter-only, auto-applies with zero restarts, exempt from the
apply lock). The capture path — turning a live edit into a commit
automatically — belongs to the agent/voice surface ("voice-initiated
changes commit with the transcript as the message") and is deferred; v1
actor enforcement remains plan-time + trust.

### Pending plans and applied_commit

`homeostat plan --save` writes `plans/pending/{id}.plan` — TOML with
`id`, `actor`, `created` (RFC3339), `base_commit`, `tier`, and the full
rendered plan text, readable on a phone as-is. `homeostat apply --plan
<file>` refuses when `base_commit` is not the repo's current HEAD (the
auto-invalidation), otherwise recomputes the plan fresh against worktree
+ bus — the file is a review artifact, not an execution script. Approval
UX beyond this arrives with the agent surface.

`applied_commit` exists only when the house root is itself a git worktree
root (`git rev-parse --show-toplevel` == the house root — a nested
fixture directory must not inherit the enclosing repo's HEAD). Then the
CLI passes HEAD (suffixed `-dirty` when the worktree has uncommitted
changes) with the apply request and the supervisor publishes it at
`home/meta/system/applied_commit` after a fully applied walk. A non-git
house applies fine but records no commit and cannot save pending plans.
Integration tests git-init fixture copies in temp dirs.

### Rollback

Git does the time travel: check out the previous commit and run a normal
forward plan/apply. Plan never reads arbitrary commits itself; it stays a
function of worktree + bus.

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
 
### Step 6 goal (settled 2026-07-04, before implementation)
 
An MCP server through which an agent can observe the house and change it,
with authority bounded by the same plan/apply machinery as every other
actor. Rides entirely on step 5b: tier derivation, pending plans, the
supervisor-executed walk.
 
- **Where it lives:** in the Rust core, `homeostat mcp`. Two transports:
  stdio for local development (the MCP client launches the binary with the
  house root and `--bus`), and HTTP for the deployed house. Deployed, the
  MCP server is a **service unit** — `units/mcp.toml` with
  `command = homeostat mcp --http <addr>` — so when `homeostat up` runs as
  PID 1 in a container, the agent surface is supervised like any unit:
  health at `home/health/mcp`, backoff, breaker, graceful shutdown, and
  the house repo opts in by declaring it. No special casing in the
  supervisor.
- **Reads:** `read_state` serves live values from the core last-value
  cache; `read_history` queries `home/history/**`. Both are bus clients;
  the agent needs zero backend knowledge.
- **Writes go through the repo.** `propose` takes text — house-repo file
  path(s) plus new content — writes it, commits to the current branch,
  and plans. Parameter edits are repo edits: a manifest-default change
  that plans parameter-only auto-applies (zero restarts, durable by
  construction, transcript-as-commit-message falls out for free). No
  separate live set_parameter tool — one path for everything.
- **The tier gates the actor.** A plan that is behavioral or structural is
  refused at agent tier by `apply`; `propose` leaves it committed and
  saved as `plans/pending/{id}.plan`. Owner approval v1 is the owner
  running `homeostat apply --plan <file>` — no in-band approval channel.
  Unwanted proposals are reverted with git, like any commit.
- **Success criteria** (`tests/mcp.rs`, real server against a live
  supervised house):
  1. `read_state`/`read_history` return what the bus and recorder hold.
  2. A parameter `propose` within constraints auto-applies: commit lands,
     the running unit sees the value with no restart.
  3. An out-of-constraint parameter `propose` is rejected with the
     constraint named; world and repo unchanged.
  4. A structural `propose` (a grant delta) produces a pending plan and
     does not touch the world; the agent's own `apply` on it is refused.
  5. Smuggling: a manifest edit carrying a grant delta escalates to
     structural through the MCP surface — the mechanical tier derivation
     is the enforcement, not tool-level checks.
- **Non-goals:** voice, dashboard generation, Zenoh ACLs, any approval UI
  beyond the pending-plan file.
 
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
   Rust, serde types, test corpus of manifest files. DONE.
2. **Supervisor + one trivial (fake) adapter: process spawning, liveliness,
   restart with backoff, meta key space.** DONE.
3. First real adapter: Zigbee2MQTT (translating subscriber), plus the
   Python SDK bootstrap. DONE.
4. First automation (evening_lights) + clock service + live parameter
   path end to end. DONE.
5. Recorder (5a), then plan/apply proper (5b). DONE.
6. **Agent MCP surface** (goal settled above, under "Agent surface").
   THIS IS THE CURRENT STEP.
7. Voice. Deferred.
Risk lives in steps 1 and 2; everything after is accretion.
 
## Open questions (flagged, not settled)
 
- Whether `features` should gate command contents beyond SDK constructors.
  Current lean: no separate layer.
- Zenoh ACL hardening timeline.
- Dashboard: generated from manifests (parameters + entities), design TBD
  after step 4.