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
  MQTT device traffic stays on its own broker (mosquitto), bridged only by
  dialect adapters — the zenoh MQTT plugin/bridge was considered and
  rejected (2026-07-17): broker retain is load-bearing (OwnTracks
  last-known position, z2m's bridge/devices inventory) and the plugin does
  not document it; mapping device topics into the zenoh key space would
  dissolve the adapters-as-only-membrane boundary that keeps the core
  owning `home/**`; and since the supervisor IS the router, plugin mode
  would put foreign-protocol parsing inside the one process that
  supervises everything else. Revisit only if the plugin gains a
  documented retain story and the deployment is too constrained for a
  broker process.
- **Units:** every running thing is a unit: `adapter`, `automation`, or
  `service`. Uniform manifest schema, uniform supervision. Python units are
  uv-run scripts with PEP 723 inline dependencies (one hermetic venv per
  process). Rust units are compiled binaries. The unit is the atom of
  authority, failure, and change; how many rules a unit script hosts is
  the author's call (see Unit granularity).
- **Process model:** plain OS processes supervised by the core
  (Erlang/actor-model lineage: fault isolation and language boundaries, not
  microservices). NOT containerized internally. The whole system may run
  inside ONE container as a deployment boundary on a shared host; the image
  then runs tini as PID 1 (reaping orphans, forwarding signals) with the
  core as its child — the core itself does per-unit process-group
  termination and sweeps, not global reaping. Host networking is
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
  a unit's descendants (a shell wrapper's child, a relay a unit manages)
  never outlive its leader, so a survivor can't keep the liveliness token
  alive and poison the next incarnation.
- Environment: `HOMEOSTAT_UNIT` (the unit's name) and `HOMEOSTAT_BUS` (the
  Zenoh endpoint to connect to, e.g. `tcp/127.0.0.1:7447`).
- **`uv run` is resolved away at spawn** (settled 2026-09-08). For a
  command of the form `uv run [flags] <script.py> [args]` the supervisor
  runs `uv sync --script` and then `uv python find --script` in processes
  that exit, and execs the environment's interpreter on the script
  directly. The interpreter is the unit's process-group leader and the
  direct holder of `PR_SET_PDEATHSIG`; there is no long-lived wrapper. The
  PEP 723 block stays the single authority on dependencies, `files_hash`
  still covers the pin, and the manifest still says `uv run` — the
  resolution is a supervisor mechanic, not a manifest contract. Resolution
  happens per incarnation, so a restart after an SDK bump picks up the new
  environment. If uv cannot resolve the script the original command is
  spawned unchanged, so a broken script fails exactly where and how it did
  before. Verified on the evening fixture: three interpreters as direct
  children of the supervisor, each its own group leader, nothing between.

  Why (measured 2026-08-29 on a live seven-unit house and reproduced in
  the release image): the `uv run` parent stayed alive for the unit's
  lifetime doing nothing but `wait()`, and its `Pss_Anon` scaled with
  what the unit's environment CONTAINED, on every run — not with whether
  that run installed it. Warm, uv 0.9, same machine:

  | unit shape | warm parent |
  |---|---|
  | paho-mqtt + git-pinned SDK | 3.8 MB |
  | aioesphomeapi + zeroconf + git-pinned SDK | 38.4 MB |
  | aioesphomeapi + zeroconf + PATH-pinned SDK | 4.0 MB |

  The git-vs-path source was the dominant factor for a heavy environment,
  which is why in-repo benching missed it entirely: `adapters/` use a path
  source and every vendored house used a git one. A live house measured
  223 MB of 443 MB in these parents. The earlier `uv sync --script`
  prewarm (v0.8.0) removed only the COLD-install spike, measured at
  ~15-25 MB on a real house, NOT the ~180 MB its commit message claimed;
  the wheel-distributed SDK (see SDK distribution) reached the git-source
  share; exec'ing the interpreter removes the parent altogether, warm or
  cold, whatever the source. A second thing it closes: `PR_SET_PDEATHSIG`
  only ever reached the direct child, so a SIGKILLed supervisor left the
  interpreter under a dead `uv run` orphaned. With the interpreter as the
  direct child, the kernel reaches it.

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

Broker credentials (revised 2026-08-29): `${VAR}` in the endpoint still
works and stays the lighter option, but URL syntax cannot carry every
password — `@`, `/` or `#` in one silently reparses the host rather than
failing — so `HOMEOSTAT_MQTT_CREDENTIALS` may instead name a TOML outside
the repo, keyed by broker hostname, read in the SDK so every MQTT adapter
gets it. Inline credentials win when both are present. A broker that
requires auth must never force its password into a unit manifest.

Entity binding for z2m: the entity file's `id` is the Zigbee2MQTT topic
segment (`{base}/{id}` — the friendly name or IEEE address), the file
stem is the bus entity name, `room` comes from the entity file. The base
topic is the endpoint's path (`mqtt://broker:1883/VP52/zigbee2mqtt`),
defaulting to `zigbee2mqtt` (revised 2026-08-29 — an estate that has run
a non-default prefix for years cannot move it, because Home Assistant and
Node-RED address it directly). It is not a secret, so the repo is its
place; it is not runtime-tunable, so it is not a param. The adapter
subscribes `{base}/+`, which keeps `bridge/#` traffic out and means
friendly names containing `/` are unsupported.

House-wide inputs (added 2026-08-29, from a live standup): change
detection is per-unit — a unit's `files_hash` covers its command, its own
entity files and its zone. The dashboard breaks that assumption, because
its model is a view over the WHOLE house: entities bound to another
adapter change what it should render while changing none of its own
files, so `apply` restarted the adapter, reported success, and left the
page confidently wrong — rendering, responsive, missing a room that
exists. A unit declares `[unit] inputs = "house"` (default `own`) and
every manifest, every entity file and `zones.toml` feed its hash. The
dashboard also rebuilds its model per `/api/model` request, keeping the
last good one if a rebuild fails, so a browser refresh suffices even
without a restart. `mcp` needs neither: it re-reads the repo per request
already.

An unbound device is not a dropped message (revised 2026-08-29, from a
live 12-device bridge): `unknown-device` fires only when the device is
not in the adapter's own discovery view — for z2m, absent from
`bridge/devices` entirely; for OwnTracks, whose view grows incrementally,
on first sight and then never again. A device the bridge knows but no
entity file binds is a steady state that discovery already reports with
`configured: false`, and the discovery-first workflow makes it the normal
condition for a house mid-configuration. Reporting it per publish
measured 107 events/hour from ONE device, forever, into the recorder's
store.

A wrong base topic is the failure worth designing against: the
subscription SUCCEEDS and matches nothing, so there is no SUBACK timeout
and no error — an adapter permanently deaf while reporting healthy, the
shape of bug this project keeps finding. The retained
`{base}/bridge/devices` inventory is therefore proof of life: silence
past `inventory_timeout_s` (parameter, owner-editable, default 30 s)
emits one `bridge-silent` health event naming the base topic in use.

That covers boot. Mid-run liveness rides the bridge's own retained
`{base}/bridge/state` (online/offline), one `bridge-silent` per down
transition — the ivt490 `device-silent` precedent. A re-arming inventory
timer would be the WRONG mechanism and was rejected: z2m republishes
`bridge/devices` only on change, so its silence cannot distinguish a dead
bridge from a stable estate and the timer would fire on a healthy one.

### Bus payload conventions

Payloads on `state` keys are bare JSON values; `cmd` keys carry the cmd
envelope `{value, priority, actor}` (see Arbitrated mode).

State: a z2m JSON object fans out per top-level field to
`home/state/{room}/{entity}/{field}`. The z2m `state` field is normalized —
adapter-native vocabulary does not leak onto the bus:

- lights/switches: aspect `on`, boolean (`"ON"` → `true`)
- locks: aspect `locked`, boolean (`"LOCKED"` → `true`)

Other scalar fields pass through under their z2m names (`brightness`,
`temperature`, `occupancy`, ...). Composite fields (objects/arrays, e.g.
`color`) are deferred.

Commands: the payload on `home/cmd/{room}/{entity}/{aspect}` (and, for
arbitrated entities, `home/arbiter/{room}/{entity}/{aspect}`) is the cmd
envelope; the adapter unwraps its `value` and translates the same way
either way. `on` + boolean value becomes `{"state": "ON"|"OFF"}`; `locked`
+ boolean value becomes `{"state": "LOCK"|"UNLOCK"}` (z2m's lock vocabulary
is asymmetric: state reports are `LOCKED`/`UNLOCKED`, but set commands are
`LOCK`/`UNLOCK`); any other aspect passes through as `{aspect: value}` to
`zigbee2mqtt/{id}/set`.

Locks are commandable only via the arbiter's output key: plan-time
expansion gives the adapter's templated `home/cmd` subscription only its
non-arbitrated bound entities, and its templated `home/arbiter` subscription
only the arbitrated ones (locks, today) — the adapter physically lacks a
cmd path to an arbitrated entity, by expansion, so a wish can only reach it
after clearing the arbiter's lease.

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
- `home/state/**` — the same mirror generalized (the agent surface and
  the SDK's `subscribe` catch-up read it). Each reply's attachment is the
  value's age in seconds since the mirror received it.

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
  the handler gets `(key, value)` with the JSON payload decoded. Subscribe,
  then get, merge, as for config: the current value of every matching key
  is read from the core's state mirror and delivered before the call
  returns, so a restarted unit is not blind until its sources publish
  again. A handler declared `(key, value, age_s)` also gets the value's
  age in seconds — zero for a live sample, the mirror's age for a
  catch-up — to pass to `Freshness.seen`; a two-argument handler gets the
  catch-up as though it had just arrived.
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
 
- `class`: `state`, `cmd`, `config`, `meta`, `health`, `clock`, `history`,
  `discovery`.
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
system. The designated growth path is tiering, not an engine swap: hot
weeks stay in SQLite, closed months roll out to Parquet files, and DuckDB
reads across both. Both engines stay embedded, there is still no server,
and the tests still only ever touch SQLite. QuestDB was the earlier
designation and is withdrawn: it is JVM-based, which is exactly what this
section refuses. DuckDB was considered and rejected as the store —
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

Scalar samples from room/entity/aspect keys, normalised so the file is
bounded by sample count rather than by repeated strings — the store layout
version is stamped in `PRAGMA user_version` and `init_store()` migrates an
older file in place (version 0 was one wide `samples` table with every
tag as TEXT on every row: measured at ~113 bytes a row against ~25 here):

```sql
series(id, class, entity, aspect)     -- UNIQUE (class, entity, aspect)
rooms(id, name)                       -- UNIQUE (name)
samples(series_id, ts, room_id, kind, value)
  -- PRIMARY KEY (series_id, ts), WITHOUT ROWID: the table is the index
  -- ts:     µs since epoch, UTC, recorder receive time
  -- class:  'state' | 'cmd'
  -- kind:   0 bool | 1 number | 2 string; value stored natively per kind
history                               -- a view joining the three back to
  -- (ts, class, room, entity, aspect, kind, value) with kind spelled out,
  -- for anything that opens the file directly (the tests, DuckDB ATTACH)
```

Series identity is a `series` row; `room` is a tag carried per sample. An
entity move is consecutive samples whose tag changes — one continuous
series, never a new one. Two samples for one series in the same
microsecond collide on the primary key and the later one is dropped.

The file is created with `auto_vacuum = INCREMENTAL` (and a migrated file
is VACUUMed into it): it can only be set before the first page is
written, and it is what lets a future retention delete return pages to
the filesystem instead of leaving a file that never shrinks.

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
events(ts, key, payload)   -- indexes: (key, ts) and (ts)
```

### Recorded key spaces

- `home/state/**` → samples, class `state`.
- `home/cmd/**` → samples, class `cmd` — the envelope's `value` into
  samples, what was commanded, when; the full envelope into events. "Who"
  now arrives: command payloads carry actors.
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

`home/history/stats` describes the store itself in one reply:
`store_version`, `file_bytes` and `freelist_bytes` from the pager, one
`{rows, oldest, newest}` per series keyed by its history key (RFC3339,
as the samples path), and `events: {rows, oldest, newest}` (integer µs,
as the events path). It exists because choosing a retention window means
knowing what is in the file, the recorder is the only process that reads
it, and a host may have no `sqlite3` binary (2026-09-09, #25). A wildcard
over `home/history/**` fans out over series only; `stats` and `events`
answer their own keys.

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

- **The recorder catches up from the state mirror (2026-09-11, #60).**
  A unit like any other, it subscribes when it starts, and anything
  published in the seconds before — every unit's start publish on a
  boot, a transition during a recorder restart — was never recorded. A
  publish-on-change aspect that rarely changes (an availability flag)
  could have no history at all, and "no rows" read as "never
  published". Subscribe, then get, merge, as the SDK does for
  automations since #36: after subscribing, the recorder reads
  `home/state/**` from the core's mirror and enqueues what it did not
  see live. Two rules make the seed honest rather than a new kind of
  lie. The row is stamped at the value's own time — now less the
  mirror's age — never at recorder start, because a sample asserts an
  observation at its stamp and a mirrored value can be arbitrarily old.
  A series the store already holds at or after that time (a
  recorder-only restart; the live row was written before it went down)
  is left alone, give or take the milliseconds between the core's
  receipt and the recorder's. State only: commands, health and config
  are the events audit, and a mirrored current value is not an event.
  Considered and rejected: a start order that brings the recorder up
  before the rest. It closes the boot case, not a recorder restart, and
  it is the first dependency edge between units the manifest rules
  refuse — the mirror is the settled answer to late joiners.

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

### Retention (settled 2026-09-09, #19, #26)

Keep-forever was undecided rather than decided against: the log-sink
rejection reasons that retention is deployment configuration, which is
right for logs (they have a platform to be pushed to) and does not
transfer to a recorder-private SQLite file. Keep-forever also dissolves
the pressure the design relies on elsewhere — an adapter that publishes on
poll rather than on change costs nothing anyone can see (the onvif motion
flood, the 107-events-per-hour device, both caught only because someone
happened to measure). Retention makes noise cost something visible, which
pushes the fix back to the adapter.

- **Two windows, not one**: `retain_samples_days` and `retain_events_days`
  in the recorder's manifest. `events` is the audit trail — the "who" —
  the smaller table and the one worth keeping longest; `samples` is the
  bulk.
- **Default 0, meaning forever**, so no upgrade silently deletes history.
  The point is that the policy is expressible and visible, not that it
  changes.
- **Mechanism**: on the writer thread, so it serialises with flushes, a
  `DELETE ... WHERE ts < cutoff` per table every hour and whenever a
  window changes, then `PRAGMA incremental_vacuum` — what the file's
  `auto_vacuum = INCREMENTAL` (#24) was reserved for. One `purge` health
  event per purge that deleted anything, with rows per table and pages
  freed; a purge that finds nothing is silent, so retention never fills
  the events table with its own bookkeeping. A failed purge is a
  `purge-failed` event and the next attempt is an hour later.
- **The only destructive operation in the store.** Downsampling stays
  out of the recorder: the additive analytics layer (DuckDB over ATTACH)
  can roll up without deleting source rows, and a roll-up that deleted
  them could not be additive. No pluggable backend either: the moment
  `endpoint` accepts `postgresql://` the no-dual-path property dies and
  the tests hollow out.

### Integrity check (settled 2026-09-09, #27)

SQLite has no page checksums by default (`cksumvfs` is an opt-in shim),
where Postgres has `data_checksums`: a disk silently returning corrupt
data is invisible until a read happens to hit the page, and the store is
the only file in the system that would fail that way — VP52 spent three
weeks with a disk doing exactly this while every layer reported health.
So the recorder runs `PRAGMA integrity_check` every
`integrity_check_hours` (default daily, 0 disables) on its own read-only
connection — in WAL mode it never blocks the writer — the first one an
interval after start so a restart loop never hammers a large file. The
result is a health event at `home/health/recorder/event`: `integrity-ok`
with the duration, or `integrity-failed` with the first lines SQLite
reports. The event is the signal; repair or restore is the owner's call,
and proportionate as a recorder feature rather than a reason for a
different engine.
 
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

A house in a subdirectory of a larger repo was supported briefly
(2026-08-28) and reverted 2026-08-29: the requesting deployment moved to
its own repo, because a server needs the house as a real worktree and
handing it one means cloning the whole enclosing repo onto the box. A
subtree house is only useful when the enclosing repo is itself
deployable, which left the loosened guard with no user. The agent's
`propose` commits stay pathspec-limited regardless — a bare commit takes
whatever else is staged, and the agent is not a house's only writer.

### Rollback

Git does the time travel: check out the previous commit and run a normal
forward plan/apply. Plan never reads arbitrary commits itself; it stays a
function of worktree + bus.

## Manifest schema
 
TOML. One schema, three kinds. `schema = 1` versioning field at the top of
every manifest and entity file from day one. This section is the design
record with examples; the field-by-field reference is `docs/manifest.md`,
generated from the parser (see Agent surface).
 
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
  Settled 2026-07-16: the arbiter's output is its own reserved class —
  `home/arbiter/{room}/{entity}/{aspect}`, the cmd shape — so a wish and a
  grant can never be confused by a subscription, and writers keep
  publishing wishes to `home/cmd` without ever learning whether a target
  is arbitrated. Every cmd payload is an envelope
  `{value, priority, actor}`: the SDK stamps priority from the unit's own
  manifest declaration and actor with the unit name, so automation code
  doesn't change; adapters drop envelope-less commands with a health
  event; the arbiter forwards the envelope unchanged. The write token is
  a lease per (arbitrated entity, aspect) — amended 2026-07-18 from
  per-entity when the heat pump showed why: orthogonal control
  dimensions share an entity (the family adjusts `setpoint`, the price
  automation continuously drives `outdoor_temperature_offset`) and must
  not block each other, and the aspect is already the granularity of
  the cmd key itself. A winning command holds its aspect at its band
  for `hold_minutes` (an arbiter parameter, family-editable);
  equal-or-higher bands pass and take the hold — a takeover from a
  strictly lower band publishes a preemption event — lower bands are
  refused with an event; expiry reopens the entity to automations, so a
  forgotten override self-heals. Arbiter events land at
  `home/health/arbiter/event` and are recorded like any health event.
  Plan-time structure: an adapter's templated cmd subscription expands
  only over its non-arbitrated bound entities, and a templated
  arbiter-class subscription expands only over the arbitrated ones — an
  adapter physically lacks a cmd path to an arbitrated entity, by
  expansion. An arbitrated entity not covered by some unit's
  arbiter-class publish is a plan error.
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
 
The HTTP transport carries the same three gates as the dashboard (added
2026-08-29, reviewing it against Local-only access): `Host` non-global or
known, `Origin` absent or allowed, and `X-Homeostat` on every request. It
had none of them, and the design's own reasoning applies with more force
here than to the dashboard — this surface writes and commits to the house
repo. Without the header a cross-origin `text/plain` POST is a CORS
"simple request": no preflight, so a page in a family browser could drive
`propose` blind. `HOMEOSTAT_MCP_HOSTS` extends the name allowlist. An
HTTP MCP client must send the header; stdio is unaffected.

Tools: `read_state`, `read_history`, `read_logs`, `read_events`, `propose`,
`plan`, `apply`, `explain`, `schema`. The agent never touches the bus directly for
structural work; it manipulates text and goes through plan/apply like every
other actor.

**Error codes are the contract's rules, served in-band (2026-09-07, #4).**
Every validation failure carries a stable code, and `src/error.rs` holds the
one registry mapping each code to a paragraph: the rule and why it exists. A
test asserts the registry and the codes the source emits are the same set.
A refused plan or propose appends the paragraphs for the codes it hit, the
`explain` tool and `homeostat explain <code>` serve them on demand, and no
code without one is fitted.

**The manifest contract is the parser, served (2026-09-07, #4).** The
structs in `src/manifest.rs` are the complete spec (`deny_unknown_fields`),
so they derive a JSON Schema; field doc comments are the descriptions,
which puts the rule next to the field it constrains and nowhere else.
`homeostat schema` and the MCP `schema` tool serve it, and
`docs/manifest.md` is the same schema rendered — generated by
`homeostat schema --markdown`, pinned by a test that refuses a stale copy.
Hand-written reference documentation was rejected: it would be a second
copy of the structs, and the drift it invites is the problem #4 reports. Agent-authored parameter
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
 
### Settled during step 6
 
- **The protocol layer is hand-rolled** (~200 lines): initialize,
  tools/list, tools/call, ping over JSON-RPC 2.0 — newline-delimited on
  stdio, stateless streamable-HTTP on `--http` (POST answers
  `application/json`, which the spec permits in place of an SSE stream;
  GET is 405 because this server never initiates messages, so there is no
  session to manage). An MCP SDK would have been the largest dependency
  in the tree for five methods.
- **The core now mirrors `home/state/**`** into a last-value queryable —
  the clock mirror generalized. read_state needed current values to be
  readable on demand; every late joiner benefits, not just the agent.
- **The enforcement point for agent parameter edits is plan-time
  validation.** The validator rejects a default outside its own
  constraint (`invalid-default`, pinned in the corpus), so propose
  refuses the edit before anything is committed. Previously an
  out-of-constraint default would have seeded the config store silently —
  a real gap the agent surface exposed.
- **Entries under `plans/` are excluded from head_commit's dirty check.**
  A pending plan is a review artifact of the commit it plans against;
  before this, saving one marked the repo dirty and made `apply --plan`
  refuse the very plan it had just saved as stale.
- **Propose is write → validate → restore-on-failure.** An invalid
  proposal never reaches a commit and the working tree ends clean either
  way. Proposed paths must be plain repo-relative (no `..`, nothing under
  `.git/` or `plans/`).
- **Agent commits are authored `homeostat-agent <agent@homeostat.local>`**
  with the propose message as the commit message — the same channel the
  voice phase will use for transcript-as-commit-message.
 
## Discovery (settled 2026-07-05)

The `discovery` class carries an adapter's complete current view of its
periphery — what the protocol can see that the house has not claimed.
Contract: an adapter that can enumerate its devices publishes one JSON
array at `home/discovery/{unit}`, each record carrying

- `id` — the exact value an entity file's `id` field must use to bind
  the device; only the adapter knows its own binding rule, so agents
  never guess it;
- `configured` / `entity` — whether an entity file already binds it,
  and which;
- `suggested` — a best-effort `{capability, features}` stanza in
  homeostat vocabulary, or null: the adapter suggests, the plan/apply
  review decides. A hard mapping would make unknown device types
  invisible; agent-side-only mapping would push a per-protocol table
  into every agent;
- `description` — the raw protocol descriptor verbatim (for z2m: the
  definition with its `exposes`), so richer consumers can dig;
- `aspects` (optional, bound records only) — the entity's aspect
  descriptor for the dashboard (see "Aspect descriptors (settled
  2026-09-08)").

Decisions and why:

- **One key, whole inventory.** Device ids may contain `/` (z2m allows
  hierarchical friendly names), so per-device key segments are a trap;
  a complete document per publish also makes departures trivial and
  matches the consumer (an agent reads everything, filters
  `configured = false`).
- **Core stays thin**: the class name in the schema, a supervisor
  mirror of `home/discovery/*` for late joiners (what read_state
  reads), nothing else. Same shape as `health`: plumbing in core,
  content from units. How discovery happens (retained bridge topic,
  mDNS browse, passive sniffing) is protocol business the core never
  sees; the manifest's `[discovery]` section configures the mechanism,
  this class carries its results.
- **Opt-in.** Adapters with nothing to enumerate (Modbus-style static
  buses) and non-adapters simply do not declare the publish.
- Out of scope, deliberately: rooms (physical knowledge no protocol
  has — the agent asks or proposes a guess for review); actuating
  discovery (permit-join, commissioning — commands with authority
  implications, grant territory for later); inventory history (the
  recorder's typed-series model does not fit an array document;
  read_state covers the agent workflow).

The agent loop this enables: `read_state home/discovery/{unit}` →
propose entity files for unconfigured records → structural pending
plan → owner applies. The agent never touches the native bus.

## Dashboard (settled 2026-07-15)

The dashboard is an adapter for humans: a supervised unit like any
other, whose protocol is HTTP + WebSocket toward browsers instead of
MQTT toward radios. Browser ↔ dashboard unit ↔ bus; browsers never
speak Zenoh.

Decisions and why:

- **Local-only access.** LAN, or WireGuard for mobile/remote devices;
  network reachability is the credential. Which is why the BUS is the
  port that matters most (added 2026-08-29, from a live deployment):
  a cmd envelope's `priority` and `actor` are self-declared and validated
  only for shape, so anything that can publish on 7447 can command every
  entity, outbid the arbiter by claiming the top band, and forge state.
  The starter therefore does not publish it — and note `127.0.0.1:7447`
  is not a boundary either, since a container on `network_mode: host`
  shares the host's loopback, which is how the exposure was found. No accounts, no login, no
  TLS. Two consequences worth recording: the browser is not local even
  when the dashboard is — a public website in a family browser can fire
  requests at LAN addresses (DNS rebinding / CSRF), so the unit
  validates `Host`, checks `Origin` on the WebSocket, and requires a
  custom header on writes, from day one, precisely because there is no
  other gate. And no PWA for now: browsers demand a secure context for
  service workers even on private addresses, so it is plain http and a
  bookmark. A private CA is a plausible later path (WireGuard
  onboarding already touches every device once); nothing architectural
  depends on the choice. Deferred.
- **Family tier only, forever.** Anyone on the network is `family`. No
  owner mode, no admin panel, no approval surface; the owner acts
  through git and the CLI. This is structural safety, not policy:
  nothing structural is reachable from the dashboard, so a stolen phone
  inside the perimeter can nudge setpoints and flip lights, not rewire
  the house. The dashboard never grows an owner surface.
- **Mediated, not raw bus.** Browsers speaking Zenoh directly (the
  remote-api plugin) was rejected: it punches past the grant table, the
  arbiter, and the manifest-declared surface, and couples every client
  to the bus protocol. Through a unit instead: commands leave at
  `priority = manual`, so THE FAMILY ALWAYS WINS falls out of the
  arbiter design, arbitrated entities included; parameter edits are
  publishes to `home/config/{unit}/{param}` validated by the existing
  live-parameter machinery; a freshly opened page snapshots from the
  core's last-value state mirror (the dashboard is just another late
  joiner) and streams deltas over the WebSocket after that; charts
  query the recorder over `home/history/**`. Almost the entire backend
  is existing plumbing.
- **Manual band vs exclusivity.** The dashboard needs a blanket
  `home/cmd/**` publish, which the two-writers-on-an-exclusive-entity
  plan error was not designed for. Settled: exclusivity constrains the
  automation band only; manual-band units sit above it by construction.
  Voice satellites inherit this same answer.
- **The dashboard honours its own grant table (settled 2026-09-07, #11).**
  Grants resolve at plan time and nothing on the bus re-checks them, so
  a blanket `home/cmd/**` publish granted for `light` could still carry a
  `climate` setpoint if the unit chose to send one — and the dashboard
  did, gating only on the capability's vocabulary. Settled: the dashboard
  derives the capabilities it may command from its own manifest's
  cmd-class publishes, refuses `/api/cmd` for any other, and marks each
  entity `commandable` in the model so the page renders ungranted
  controls inert. This is the unit keeping its declaration, not a
  boundary: a unit that opens its own session can publish anything. If
  the grant table is ever to constrain rather than describe, that is a
  bus credential per unit, not a check in each adapter.
- **Group actions are manual-edge fan-outs (settled 2026-07-26).**
  "Darken the whole house" is family intent over a set of entities, so
  the fan-out happens at the manual edge: `POST /api/lights/off` sends
  one manual-band off-command per bound light through the dashboard's
  existing blanket publish, surfaced on `Now` as the corrective action
  on the lights-on deviation (the button exists exactly when there is
  something to darken). Routing it through a commandable "scene" entity
  was rejected: the owning automation would re-publish at the
  automation band — demoting family intent below arbiter holds, THE
  FAMILY ALWAYS WINS breaking precisely when the family pressed the
  button — and would collide with exclusivity as a second
  automation-band writer on every exclusive light. Voice inherits the
  same answer: fast-path grammar → manual-band fan-out at the voice
  edge.
- **Purely generated from manifests; layout state exists nowhere.**
  Grouping from the entity `room` field and `zones.toml`; entity
  widgets derived from `capability` + `features` (a light with
  brightness/color_temp renders toggle + slider + temp control, a bare
  sensor renders value + sparkline); parameter controls derived from
  constraint types (min/max → slider, after/before → time picker, enum
  → segmented control). Every parameter is visible — an owner-level
  tuning constant reads in the unit overlay against its manifest default
  and counts as a deviation when off it — but only `editable_by =
  "family"` parameters get a control, and `/api/param` is the write gate
  (revised 2026-09-07, #10: hiding owner params made a house running off
  its manifest indistinguishable from one running it). Names and locale from `[naming]` — dashboard quality is a
  function of manifest hygiene, auditable by the agent, exactly like
  voice. Health (`home/health/**`: unit status, circuit breakers) is
  family-visible by design. If generated turns out bland,
  the escape hatch is ordering/pinning hints as text in the house repo
  — never browser-side customization, which is exactly the hidden UI
  state the project exists to reject.
- **A Python unit on the SDK**, like the other adapters, serving an
  embedded static bundle: one small SPA (Preact/Lit-scale, no
  build-time empire), fine-grained DOM updates off the WebSocket. Live
  state push is the dashboard's whole job, so client-side reactivity is
  unavoidable; server-rendered-with-sprinkles was rejected on those
  grounds.

Settled after wireframe review (2026-07-15, sheets in
`docs/wireframes/` — the hybrid sheet is the direction; A and B are
the exploration that produced it):

- **Shape: four generated views** — `Now`, `Setpoints`, `Rooms`,
  `Health`, with health also summarized in the nav rail. `Setpoints`
  is every family-editable parameter in the house as one flat list:
  the family's levers. `Rooms` is the spatial room-card grid. **`Now`
  shows the error signal, not an inventory**: people, a few key
  signals with today's range, and one deviations feed drawn from four
  sources — supervision events, arbiter preemptions, notable state
  (lights on, doors open), and setpoints differing from their manifest
  default. A house in equilibrium renders a nearly empty page,
  deliberately. What counts as "notable state" is per-capability
  vocabulary in the public schema, never house configuration.
  Revised 2026-09-09, from living with it: the MVP's "key signals"
  were the first six numeric sensor aspects in model order, and its
  "People" tile listed motion sensors — an inventory by another name,
  and it read as random because nothing said what was key. Now `Now`
  pins nothing by default: a reading is a signal tile only when its
  entity file says `[dashboard] pin = true` — the ordering/pinning
  escape hatch below, used for the first time — and the People tile is
  the `person` entities, home or away from a `presence` aspect on the
  person (new vocabulary; published by whichever adapter knows, a
  geofence transition or a fused sighting), falling back to the age of
  the last fix. Motion sensors are rooms' business. A pinned entity's
  tiles are its sensor-card rows (2026-09-10, after #56): the
  descriptor's readings in field order, never its diagnostics or a
  control — a pinned thermometer is a temperature and a humidity tile,
  not a link-quality one. The page's body is
  the deviations feed; a house in equilibrium with no one pinned is
  people plus "In equilibrium", which is what this bullet promised.
- **English first.** `[naming]` already carries `en` and `sv`; the
  dashboard renders `en` now, and locale becomes a per-browser choice
  later. No architecture in it.
- **The dashboard owns rendering; adapters never do.** Same shape as
  the discovery settlement: adapters speak homeostat vocabulary
  (capability, features, aspects, constraints); the mapping from that
  vocabulary to widgets lives in the dashboard alone. Per-adapter UI
  (the Home Assistant path) was rejected — widget drift, and the
  dashboard stops being a pure function of the house's text. A device
  class needing a new widget means extending the versioned schema
  vocabulary plus one dashboard widget: public repo, reviewed, every
  adapter benefits. Unknown aspects render generically (read-only
  value, history if recorded) rather than becoming invisible.
- **Map and person entities.** OwnTracks (or similar) is just another
  adapter, binding `person` entities that publish location aspects.
  The map is the first widget that is a view over every entity with a
  location aspect rather than a per-entity row; it appears on `Now`
  iff any exist. The key-space reservation this forces: persons move,
  so the room-keyed space gains one reserved pseudo-room — person
  entity files set `room = "person"` and their state lives at
  `home/state/person/{entity}/…`; `person` can never be a physical
  room or appear in a zone. Which physical room a person is in, when
  derivable, is state, never structure. Map tiles are self-hosted (a
  PMTiles region extract served by the dashboard unit — no tile
  server): fetching public tile CDNs would leak family positions as
  tile coordinates, exactly what local-only exists to prevent.
  Settled 2026-07-16: location is scalar aspects (`lat`, `lon`,
  `accuracy`, `battery`, and `fixed_at`, the fix's epoch timestamp
  from OwnTracks `tst`), not one composite object — the recorder
  stores scalars only, so per-aspect keys make position history free;
  atomicity of a fix was judged worth less than trails. OwnTracks
  reaches the house over MQTT via the existing broker (the
  zigbee2mqtt pattern), not HTTP mode — retained messages give
  last-known position across restarts and no second ingress surface.
  The map library (Leaflet + protomaps-leaflet) is vendored into the
  repo and served by the dashboard unit at `/assets/` — dashboard.html
  stays a hand-editable file and runtime stays fetch-free, but the
  strict one-file property is traded away. The same trade later
  (2026-07-31) extracted the page's pure decision logic (the Now-view
  deviation rules, WebSocket store application, presence-key parsing)
  into `assets/dashboard-logic.js` so `node --test tests/js` — Node's
  built-in runner, zero packages — can pin it; the DOM wiring stays in
  the page and stays untested by design.

## ESPHome adapter (settled 2026-07-16)

- **Native API, not MQTT mode**: TCP 6053 via aioesphomeapi — no broker
  dependency, matches encryption-default device configs, and the same
  dialect serves the voice satellites later. The adapter is asyncio (the
  dashboard's precedent), one connection per bound device with the
  library's reconnect logic.
- **Entity binding**: `id = "{device}/{object_id}"` — the OwnTracks
  two-segment shape. The device half resolves via mDNS
  (`{device}.local`) by default.
- **Credentials**: `HOMEOSTAT_ESPHOME_DEVICES` points at a TOML file
  outside the repo carrying each device's Noise PSK and an optional
  host override; a device without an entry is assumed plaintext.
  Device addresses and keys never enter the repo (the boundary test).
- **v1 vocabulary, grown by need**: switch, light (brightness /
  color_temp features), sensor, binary_sensor — motion/occupancy device
  classes normalize to presence, the z2m rule (adapter-native
  vocabulary does not leak onto the bus). Unmapped types land in
  discovery carrying their raw type: visible, not translated.
- **Discovery**: the adapter connects only to bound devices; an mDNS
  browse of `_esphomelib._tcp` is best-effort input to the
  home/discovery/{unit} feed (every seen device, bound or not, with a
  suggested stanza), never a prerequisite for the bound connections.
- **Commands**: cmd envelopes exactly like z2m; an arbitrated ESPHome
  entity gets the arbiter-output subscription by the same plan-time
  expansion rule. Nothing new.

## IVT490 heat-pump adapter (settled 2026-07-18)

- **The bespoke firmware stays.** The IVT490 is interfaced by the owner's
  own ESP8266 board (serial read of the control board, GT2 digipot
  emulation, EXT_IN relay — github.com/freol35241/IVT490-interface-esp8266),
  speaking its own MQTT dialect. Unlike ESPurna, there is no drop-in
  replacement and the logic is house-specific hardware knowledge: the
  dialect boundary settlement says the adapter absorbs it as-is.
- **Climate vocabulary**: the capability's family-facing base aspect is
  `setpoint` (indoor target, °C) — the climate analogue of `on`/`locked`.
  The adapter also normalizes the current readings it can derive to
  `indoor_temperature` and `feed_temperature`; every other state
  parameter passes through under its firmware name. Expert knobs
  (feed_temperature_target, outdoor_temperature_offset, operating_mode)
  are commandable aspects under their own names — dialect-specific, not
  schema vocabulary. The adapter tracks the firmware actually deployed:
  the GT3_2_boiler_emulation branch (2026-07-18), whose controller adds
  operating_mode (1 BAU / 2 BLOCK / 3 BOOST, the GT3_2 boiler-sensor
  emulation) and has no vacation command — vacation is a read-only
  state field there.
- **The heat pump is an arbitrated entity** — it is the second name in
  the arbitrated-mode sentence. All commands ride the arbiter; the
  family's manual setpoint wins.
- **Bounds live in the adapter**, as constants (setpoint 10–30 °C, feed
  target 20–60 °C, outdoor offset ±10 K): device physics is dialect
  knowledge, not house config. Out-of-range commands DROP with an
  invalid-command health event, never silently clamp — same ethos as
  every other adapter, and defense in depth over the firmware's own
  clamps.
- **Dashboard v1**: a minimal climate widget — setpoint with ±0.5 °C
  steppers at the manual band, current temperature readout when the
  normalized aspects are present; expert knobs stay read-only in the
  entity detail overlay with history. Superseded in the overlay
  (2026-09-08) by the adapter's aspect descriptor — see "Aspect
  descriptors": labelled, grouped readings, the owner knobs (mode
  included) badged.
- **MQTT boilerplate graduates to the SDK** (`homeostat.mqtt`): this is
  the third paho adapter, the agreed rule-of-three trigger. A helper
  function, not a transport layer — adapters still own their
  connections.
- Operational note: any Node-RED flow WRITING to the interface's
  controller/set topics must be disabled when this adapter goes live —
  one master per device. Read-only flows can coexist.

## Aspect descriptors (settled 2026-09-08)

The dashboard renders parameters well and aspects badly, for one reason:
a param arrives with a type, a constraint and an `editable_by`, and the
page has a small engine turning that into a slider, a segmented control
or a read-only value with a tier badge; an aspect arrives with a name
and a value. The heat pump made this concrete — thirty-odd rows of raw
firmware names in the detail overlay, and no way to reach the expert
knobs the adapter takes commands for. A hand-built IVT490 panel was
rejected on the settled rule that the dashboard owns rendering and
adapters never do. The gap is metadata, so the fix is metadata.

- **An adapter may describe an entity's aspects**, in the same
  vocabulary the schema already uses for params: per entity, a
  `{schema, groups, fields}` document where each field carries `label`,
  `kind` (`temperature`, `temperature_delta`, `percent`, `number`,
  `boolean`, `enum` with `values: [{value, label}]`), `group`, an
  optional `valid` naming the boolean aspect that marks the value
  stale, an optional `notable` flag, and — for aspects the adapter takes
  commands on — `command: {type, constraint, step?, editable_by}`, the
  ParamSpec fields verbatim. The firmware names never become schema;
  they get labels.
- **It rides the discovery record.** The descriptor is the `aspects`
  member of the entity's record at `home/discovery/{unit}`. Considered
  and rejected: a key under `home/meta/` (core-owned: the supervisor
  serves that whole space to late joiners, so a unit publishing there is
  invisible to a fresh reader) and a new class (a second self-description
  document per adapter, with its own key shape, for the same purpose
  discovery already serves — the adapter describing its devices in
  homeostat vocabulary). Discovery is mirrored, declared, and already
  per-entity; the dashboard lifts descriptors out of it and forwards
  only those to browsers. Nothing in core changes.
- **The dashboard still owns every widget.** It maps descriptor
  vocabulary onto the param-control shapes it has (float with a step →
  stepper, other numbers → slider, enum → segmented control, owner tier
  → value with badge) and groups rows as the descriptor says, with every
  undescribed aspect demoted to a collapsed diagnostics group rather
  than hidden. An undescribed entity renders exactly as before. The
  mapping is pure and pinned by `node --test tests/js`.
- **Commands widen by the same rule that gates params.** `/api/cmd`
  admits an aspect the descriptor declares a family-editable command
  for, checked against the declared constraint — a courtesy before the
  bus; the adapter's own bounds remain the enforcement, and the grant
  table (the capability, from the dashboard's own manifest) is checked
  first, unchanged. Owner-tier commands read in the overlay and are
  written only through the bus. For the heat pump: the indoor target is
  family intent; feed target, curve offset and the GT3_2 emulation's
  mode are owner tuning — the mode is driven by an automation at the
  reporting house, and a knob an automation owns is not a family lever.
  Labels keep the firmware's sensor code, "outdoor (GT2)", so the page
  and the pump's manual name the same thing.
- **`notable` is a deviation source.** A described boolean marked
  notable that reads true (the pump's alarm flag) lands on `Now` as an
  entity deviation — the adapter declaring vocabulary, still never house
  configuration.
- **The room card follows the descriptor too (2026-09-08, #32).** A
  described entity's card row is name plus the first family control on
  one line, up to two headline readings under it, the arbitrated badge
  on the readings line. Headline is a convention, not vocabulary: the
  first two control-less rows of the first group that has any, so the
  adapter's own field order decides. A `headline` flag was considered
  and deferred until an adapter needs to say otherwise. Card labels
  drop the trailing firmware code the overlay keeps. Strike one
  (2026-09-09, #53): on a live house every z2m thermometer headlined
  `battery`, because z2m lists it first and the generator promoted it
  into readings without saying where — and on z2m before 1.34 no expose
  carries a `category`, so `voltage` and `linkquality` were readings
  too and diagnostics was always empty. Answered inside the convention,
  not with the flag: a generator that bends the group by property name
  (battery) bends the order by the same rule (battery last), and the
  diagnostics newer z2m categorises are known by property when the
  field is absent (linkquality; voltage in mV, since a plug's mains
  voltage in V is a reading). The flag stays deferred: the strike was
  a generator with one deliberate exception that forgot half of it, not
  a case the adapter's order cannot express. A second generator needing
  ordering logic beyond one demoted field is strike two. Strike one's
  fix never reached the room card (2026-09-10, #56): `sensor` keeps its
  bespoke sparkline widget, and that widget listed every numeric state
  key in arrival order, descriptor unread — so the thermometers still
  showed link quality beside temperature on Rooms, and had no tap that
  opened the entity detail at all (each row leads to its aspect's
  chart; only a deviation on `Now` reached the overlay). Settled the
  same way as the climate card: a bespoke widget keeps its shape and
  takes its row list from the descriptor — the numeric, control-less
  rows outside diagnostics, in field order (`sensorCardPlan`), and a
  multi-aspect sensor gets a head row naming the entity that opens the
  detail. Routing sensors to the described card was the cheaper fix
  and was rejected: two headline readings and no sparkline is a worse
  thermometer than the widget already is. A single-aspect sensor is
  unchanged, its overlay still one tap short; a head row on every fused
  virtual sensor is a cost the gap does not yet justify.
- **Reach.** Nothing here is heat-pump specific: any adapter can label
  `battery` a percent and `linkquality` diagnostics. Grown by need, not
  ahead of it. First growth (2026-09-09): the Zigbee2MQTT adapter
  generates a descriptor per bound device from z2m's `exposes` — unit
  picks the kind, category picks the group, a settable config expose
  becomes an owner-tier command with z2m's own bounds, the alarm-shaped
  binaries are notable — so no per-device label is ever hand-written.
  It forced one vocabulary addition: a `number` may carry a `unit`
  string (lqi, lux, hPa, W) the page shows after the value, because
  kinds name formatting, not physics, and the long tail of units is the
  protocol's to declare. The capability's own vocabulary (on, locked,
  brightness, color_temp) is described as readings only; its controls
  stay the dashboard's bespoke widget, never a descriptor command.
  The same day, ESPHome (generated per entity from its EntityInfo:
  unit → kind, the device's entity name → label, the alarm-shaped
  device classes notable) and OpenWrt (a static one boolean per
  capability with value labels, "up"/"down", "present"/"away" — the
  whole of what it speaks). A boolean may carry `values` naming true
  and false, the second and last vocabulary addition this round.
  ONVIF and OwnTracks publish schema vocabulary only and describe
  nothing. Locale (`{en, sv}` labels, the `[naming]` shape) is the
  obvious next step and is deferred with the dashboard's English-first
  settlement.

### Device feeds: an input wired to one source (settled 2026-09-08, #9)

The firmware's fifth set topic, `controller/set/indoor_temperature_actual`,
was reserved "for a future automation" with no plan behind the reservation.
#9 arrived with that automation built and verified and nowhere to deliver
to. Settling it also reframed the offset: at the reporting house
`outdoor_temperature_offset` is likewise written continuously by an
automation, so "feedback versus command" is not a property of the value.

- **What a feed is.** A command is discrete intent that may be contested:
  it rides the arbiter, has a band, the family can override it. A feed is a
  continuous signal with exactly one master, where the failure that
  matters is staleness, not conflict. Which of a device's inputs are fed is
  a per-house decision — the same input is a command in one house and a
  feed in another — so the wiring lives in the entity file, beside the
  other device-specific knowledge (the base topic).
- **The reference is entity + aspect, not a bus key and not a unit.** The
  house already has an identity layer between the two: entities, whose
  aspects the bus keys derive from. A derived value becomes a virtual
  entity precisely so it has that identity (recorder, dashboard,
  `read_state`); the reporter's fusion already publishes to one. The
  automation is the wrong granularity — a unit publishes several things,
  and what is consumed is one signal. The core resolves the reference to
  a key at plan time and prints it, exactly as it resolves grants.
- **Shape.** The adapter declares which device inputs are feedable (for
  ivt490: `indoor_temperature_actual`, `outdoor_temperature_offset`). The
  entity file wires them:

  ```toml
  [inputs]
  indoor_temperature_actual = { entity = "indoor_temperature", aspect = "temperature" }
  ```

  A wired input has one master by construction, so it stops being a
  command aspect for that entity; an unwired offset stays a command, as
  today. The plan validates that the source entity exists, that its owner
  publishes the aspect where that is knowable (automation-owned entities
  name their aspects literally in `[bus.publishes]`), and renders the
  edge. The automation side needs nothing new.
- **Staleness is the device's.** Each fed input carries the firmware's
  own validity window (`{value, valid}`, #12): the adapter forwards while
  the source is available and stops when it is not, and the device drops
  the term and falls back to curve control on its own. No adapter-side
  timeout; the honest signal is already on the bus as `{aspect}_valid`.
- **A feed is a dependency edge the other way round.** Grants run
  automation → device; a feed runs device → automation's entity. A
  control loop that reads the pump's state and feeds a term back is
  legitimately cyclic, and the apply walk tolerates it (ties by kind)
  rather than refusing it.
- **Rejected**: a fifth command aspect marked non-arbitrated and
  non-family (mechanically enough, and a misdescription that would put
  sensor feedback in the grant table next to setpoints); a new grant kind
  (machinery for what an entity-file reference expresses); the adapter
  subscribing a raw bus key (bypasses the identity layer the rest of the
  design leans on).

Settled on the reporter's confirmation (2026-09-08, same thread), built
in the same change:

- **Not retained, and cleared on loss.** The reporting house's Node-RED
  writer published `indoor_temperature_actual` retained, so "stop
  forwarding" would have stopped nothing: the broker keeps serving the
  last value to the pump across a reconnect, and the firmware's validity
  window becomes the only thing that ends a stale feed. A fed value is
  therefore published NOT retained, and when the source's `available`
  goes false the adapter clears the topic's retained slot once (an empty
  retained publish — which this firmware's parse discards, so it is a
  clear, not a zero) and reports `feed-source-lost`. Cutover note: clear
  the topic when switching masters, or the old writer's retained value
  outlives it.
- **One subscriber per source (2026-09-09).** The adapter first subscribed
  the value key and the `available` key separately; zenoh orders samples
  within a subscriber, not across two, so `available = true` followed by
  a value could be delivered value-first and the value silently dropped —
  a CI-only failure until the ordering was understood. One subscriber on
  the source entity's `home/state/{room}/{entity}/*` keeps both in
  publish order, and a value dropped while the source is unavailable now
  leaves one `feed-source-unavailable` drop per outage.
- **No adapter-side refresh cadence, for now.** The adapter forwards each
  source sample and nothing between samples; a transition-only source
  plus a device validity window shorter than its quiet periods is a
  house tuning question (publish on a cadence, or lengthen the window),
  not adapter machinery. Revisit if the firmware turns out to need a
  retained value after reboot.
- **The adapter is the authority on input names.** The core validates the
  reference (entity exists, an automation-owned source publishes the
  aspect, the fed entity is a device); which inputs exist is dialect
  knowledge, and an unknown one refuses to start, visibly.
- **Source ownership is unrestricted.** Nothing in the reference needs to
  know who owns the source; an adapter-owned aspect is as feedable as a
  virtual sensor's. Only the *fed* side must be a device
  (`virtual-entity-fed`).
- **A per-house decision can cut through one automation.** At the
  reporting house one computation writes both `outdoor_temperature_offset`
  (a feed under this shape) and `operating_mode` (contestable, rightly
  arbitrated), so half its output goes by feed and half by arbiter with
  no guarantee they land together. Recorded, not mechanised: the two
  halves genuinely have different governance, and coupling them would
  push feed semantics into the arbiter. An automation that needs the
  pair to move together holds the mode lease and feeds the offset
  against it.
- **Feeds are not walk-order edges.** They appear in the plan beside the
  grant table and in the manifest reference; the apply walk still orders
  by grants only, so the loop an automation closes through a device
  never needs untangling.

## Logs and the audit trail (settled 2026-07-18)

- **Unit output is captured, not inherited.** The supervisor pipes every
  unit's stdout/stderr, re-emits each line onto its own corresponding
  stream tagged `[{unit}]` — `docker logs` stays THE raw stream, now
  attributable — and keeps the last 500 lines per unit in a ring buffer
  served by a queryable at `home/meta/{unit}/log` (`?lines=N` caps the
  tail), each entry `{ts_us, stream, line}`. Logs are operational
  exhaust: bounded memory, gone on supervisor restart, never recorded —
  the events channel is the durable trail, logs are for debugging.
- **The events table gets a query surface.** The recorder's queryable
  grows `home/history/events` (same selector conventions as the samples
  path: `?key=<keyexpr>;from=..;to=..;limit=..`, key wildcards
  included), replying `{ts, key, payload}` rows — health events,
  preemptions, config writes, and cmd envelopes with their actors
  become askable, not just written.
- **Agent and family access ride the existing surfaces**: MCP gains
  `read_logs` (unit, lines) and `read_events` (key/from/to/limit); the
  dashboard's unit detail overlay gains the log tail through a
  dashboard.py proxy endpoint, the /api/history pattern.
- A unit still cannot set its own health status — `home/health/{unit}`
  stays supervisor-owned; `ready()` and health events remain the unit's
  two voices. Degradation is derived from those, never declared.
- **A log sink was considered and rejected (2026-07-18).** The
  supervisor's tagged stdout already is the standard export surface —
  the 12-factor seam: the app emits an attributable line stream, the
  platform sinks it. Durability, retention, and indexing are deployment
  configuration (Docker logging drivers: json-file rotation, journald,
  Loki), never house machinery — anything built here would be a worse
  reimplementation of mature tooling, welded on. Deeper reason: "logs
  are exhaust, events are the trail" is a design force, not just a
  storage rule — if a line matters enough to query next week, that
  pressure must push the unit to emit a structured health event, and a
  durable, queryable log store inside homeostat would dissolve exactly
  that pressure. The recorder records data; it never becomes a log
  pipeline. (Mechanically it would also mean publishing every stdout
  line onto the bus in the same traffic class as state and commands —
  nothing good lives down that road.)
- **The sanctioned extension: peripheral logs ride the adapters' own
  stdout.** A device's or bridge's log stream (ESPHome's native-API log
  subscription, zigbee2mqtt's bridge/logging topic) may be printed by
  its adapter, one line per entry tagged with the device — the
  supervisor then does the rest: `[{unit}]`-tagged docker logs, the
  ring buffer, the dashboard tail, read_logs. One pipeline, no new
  architecture. Gate at warning-and-up so a debug-chatty device cannot
  drown a 500-line ring.

## Cameras (settled 2026-07-19)

The founding decision is a plane split, the camera analogue of "logs are
exhaust, events are the trail": **pixels are the media plane, detections
are data.** Everything in homeostat is small scalar JSON — the payload
conventions, the recorder's schema, the last-value cache all assume it —
and video is a different physical medium. The moment video bytes enter a
homeostat process, the small core is gone.

- **Event plane (bus, recorded, automatable):** a camera is an entity
  like any other — `capability = "camera"`, a room, an adapter binding —
  publishing scalar aspects at `home/state/{room}/{camera}/…`. v1
  vocabulary: `motion` (bool). Automations never see pixels; they see
  `motion = true`, exactly as they see `occupancy` from a PIR. Motion
  transitions land in the recorder as ordinary state — the event
  timeline is history, the frames are not.
- **Media plane (off-bus, never recorded):** live viewing rides RTSP →
  **go2rtc**, run as a supervised `service` unit (a single static Go
  binary — the process model fits it like a compiled Rust unit).
  Restreaming is a pure remux, no transcoding; browsers never speak
  RTSP, and the bus at most carries pointers, never frames. go2rtc
  holds ONE upstream RTSP session per camera regardless of viewer
  count — load-bearing here, since Tapo caps concurrent RTSP clients
  at about two: viewers are free and `/stream2` stays open for a
  future detector. Stream names equal entity ids.
- **Browsers never speak go2rtc** — the Zenoh sentence, second verse.
  go2rtc's API is unauthenticated and structurally capable: it adds
  and removes streams at runtime, reads config back out (RTSP URLs
  with credentials embedded), and its source types include `exec:` —
  command execution. Exposing it to the LAN would hand every device
  and every DNS-rebinding attack a surface far past "nudge setpoints",
  recreating the raw-bus problem the dashboard exists to mediate. So
  go2rtc binds its API to 127.0.0.1 and the dashboard mediates, with
  the machinery it already has (Host validation, Origin-checked
  WebSockets, the /api/history proxy pattern):
  - `/api/camera/{entity}/live` — WebSocket, relayed byte-for-byte to
    go2rtc's `api/ws?src={entity}`. Transport is **MSE/fMP4; WebRTC
    is deliberately not v1**: ICE negotiates a direct peer connection
    that structurally cannot ride the proxy, to buy sub-second
    latency where MSE's ~0.5–1.5s is fine for glancing at a camera.
    Revisit only if two-way talk ever matters.
  - `/api/camera/{entity}/snapshot` — proxies go2rtc's `frame.jpeg`;
    the room-card poster. Live streams start only on tap, in the
    entity detail overlay — never N always-on streams.
  - Player: go2rtc's own `video-stream.js` web component, vendored
    into `/assets/` (the Leaflet precedent), pointed at the proxy URL.
  Video bytes do transit the dashboard unit — as an opaque socket
  relay. The plane split's force is that the bus, recorder, and core
  stay scalar; the dashboard is the declared browser edge of the
  media plane, and relaying is not processing.
- **First foreign binary as a unit — the shim owns the token.** The
  unit contract demands a liveliness token a Go binary cannot
  declare. `units/go2rtc.py` is a thin SDK shim: it reads
  `HOMEOSTAT_CAMERAS`, renders the go2rtc config (API on 127.0.0.1,
  one stream per camera, `rtsp://user:pass@host/stream1`), spawns the
  binary as a child, polls its API until healthy, and only then
  declares `ready()`. Child death → shim exit → supervisor backoff;
  the process-group sweep already guarantees no orphan. The camera
  list has one source of truth (the credentials file, keyed by entity
  id); the binary itself is image-build provisioning, never repo
  content. This shim pattern is the general answer for any future
  foreign binary.
- **Refused, deliberately** (the log-sink shape): no NVR, no motion
  detection, no transcoding, no frame storage inside homeostat. All
  four are mature-tooling territory; anything built here would be a
  worse reimplementation welded on. The cameras' own SD-card loop
  recording is the interim clip story; clips are out of scope.
- **Frigate was evaluated and is the designated growth path, not v1**
  — the QuestDB pattern. It is exactly z2m-shaped (an external bridge
  with an MQTT dialect, one adapter to consume it) and would upgrade
  the event plane to real person detection. Rejected for now on
  hardware grounds: the house server is an i3-540 (Clarkdale, 2010) —
  its Gen5 iGPU is below OpenVINO's Gen6/Skylake floor, there is no
  Quick Sync and no AVX, so both accelerated and CPU inference paths
  close. Because aspects are homeostat vocabulary (`motion`, `person` —
  never the detector's words), adopting Frigate later changes one
  adapter and zero automations; the key space is the stable contract.
- **The inventory is TP-Link Tapo C200** (indoor pan/tilt). Dialect
  facts, verified 2026-07-19: RTSP on 554 (`/stream1` HD, `/stream2`
  SD — the substream a future detector would eat), ONVIF Profile S on
  port 2020, local "camera account" credentials created in the Tapo
  app with third-party compatibility enabled. The adapter consumes
  ONVIF pull-point events for `motion` — the same source the Home
  Assistant integration uses; Tapo firmware has broken this in the
  past (1.3.6), so event-subscription loss must resubscribe/reconnect,
  not crash. **A C200 notification is not a transition**, verified
  against two of them 2026-08-29: it sends `MotionAlarm` on every
  evaluation tick, so one real motion episode arrived as 417 identical
  `true`s in 56 seconds. The adapter absorbs that as it absorbs any
  other dialect quirk: `motion` publishes on change, the producer norm.
  On-camera person detection exists but is not exposed over
  ONVIF — it is app-only, so it is NOT an aspect until firmware
  exposes it or a Frigate-class detector arrives. ONVIF on Tapo does
  no PTZ; pan/tilt and privacy mode need the vendor API (pytapo) and
  are deferred — noted for later because privacy mode ("family is
  home → lens down") is the first camera *command* worth having, and
  smells arbitrated.
- **The adapter is `onvif.py`, named for the dialect it speaks** — the
  esphome precedent (adapters are named for the dialect they absorb),
  not for the vendor. Scope stays inventory-bounded: Profile S
  pull-point events only — no PTZ, no imaging service, no capability
  negotiation. Nothing in the event path is Tapo-flavored; the
  Tapo-specific parts are per-camera facts (host, port, credentials),
  which are config, not code. A vendor adapter (tapo, via pytapo)
  exists only from the day vendor-API commands (privacy mode,
  pan/tilt) are actually wanted — and since exactly one adapter binds
  each entity, it then subsumes ONVIF events and takes the cameras
  with it by a normal plan/apply migration, rather than sitting
  beside `onvif.py`. Pre-building that path before any command exists
  is the speculative branch.
- **Credentials**: camera account user/pass and host per camera in an
  out-of-repo TOML behind `HOMEOSTAT_CAMERAS`, keyed by entity `id` —
  the ESPHome-devices pattern; addresses and passwords never enter
  the repo (the boundary test). Cameras are cloud-attached by default; they belong on a
  segment firewalled from WAN, with the app's cloud features accepted
  as lost. No cloud in any homeostat path.

## Network presence and connectivity (settled 2026-07-25)

The founding decision is a scope split, the network analogue of "pixels
are the media plane": **presence and connectivity state are house state;
network metrics are observability.** Homeostat carries what regulation
and the family consume — who is home, whether the WAN and the VPN
tunnels are up. Throughput curves, router CPU, latency histories,
per-interface counters are owner-facing diagnostics: mature-tooling
territory (Prometheus + Grafana beside homeostat, blackbox probes,
`prometheus-node-exporter-lua` on the routers), and anything built here
would be a worse reimplementation welded on — the log-sink sentence,
third verse. The same fact may surface on both sides (a tunnel down),
deliberately: homeostat renders the family-facing deviation on `Now`,
the monitoring stack the owner-facing diagnosis. No coupling in either
direction.

- **The adapter is `openwrt.py`, named for the dialect it speaks**: ubus
  JSON-RPC over HTTP (`uhttpd-mod-ubus`, rpcd session auth) — the first
  polling adapter (MQTT pushes, ONVIF long-polls; ubus answers questions).
  One adapter, many routers: `HOMEOSTAT_OPENWRT` points at an out-of-repo
  TOML keyed by router name (`host`, `username`, `password`) — the
  ESPHome/cameras pattern; the manifest's `[discovery].endpoint` is
  `${HOMEOSTAT_OPENWRT}` itself, the recorder's endpoint-as-store shape.
  Operational note: a dedicated read-only rpcd ACL login per router,
  never root. A fresh login per poll cycle; rpcd expires idle sessions.
- **Vocabulary** (two new capabilities, `router` and `vpn`):
  - `router`, aspect `wan` (bool): the netifd interface named `wan` is
    up. Entity `id` = the router's name in the credentials file.
  - `vpn`, aspect `up` (bool). `id` = `{router}/{interface}` — the
    two-segment shape. A tunnel is a **netifd interface** (standard
    OpenWrt practice; firewall zones demand it), which is what makes
    WireGuard and OpenVPN one rule apart: proto `wireguard` is up iff
    the interface is up AND the freshest peer handshake is younger than
    180 s (adapter constant — WireGuard rekeys about every 2 minutes
    under traffic; monitored tunnels must run persistent-keepalive, the
    operational note); any other proto is the interface's own up flag.
    Handshakes come from rpcd's `luci.wireguard` status call
    (`luci-proto-wireguard`, present on any LuCI-managed WG router).
  - WiFi presence: capability `presence` (existing vocabulary), aspect
    `presence` (bool), `id` = the device MAC, lowercase, `room =
    "global"` (a phone is non-spatial). A sighting is association to
    any hostapd BSS on any configured router — the union is what makes
    AP roaming invisible. Absence requires `away_delay_s` (parameter,
    family-editable, default 180) of continuous non-sighting: phones
    sleep-drop WiFi for seconds at a time, and the same debounce
    absorbs an AP reboot.
- **Presence fusion is an automation, not adapter magic.** Exactly one
  adapter binds each entity, so `openwrt.py` structurally cannot write
  onto OwnTracks-bound `person` entities — correct, not a limitation.
  Combining WiFi sightings with location into "someone is home" is
  house-specific behavior (which MAC is whose) and lives in the house
  repo as an ordinary automation consuming both.
- **Publish on transition only** (plus each entity's current value
  after the first successful poll): a poll is a read, not an event.
  The recorder then stores exactly the transitions, and late joiners
  are already covered by the core's state mirror.
- **Failure policy**: an unreachable router emits one
  `router-unreachable` health event per down transition (the
  backend-outage precedent) and its aspects go stale rather than
  false — an unreachable AP contributes no sightings, and
  `away_delay_s` is what keeps a rebooting AP from marking the family
  away. Recovery publishes whatever actually changed during the
  outage; a long outage marking everyone absent is accepted v1
  behavior, documented here.
- **Read-only, deliberately**: no cmd surface. OpenWrt can be
  commanded (reboot, guest WiFi, tunnel up/down); the pytapo rule
  applies — the command adapter surface is built the day a command is
  actually wanted, and reboot smells owner-tier.
- **Discovery** from data already fetched: associated stations
  (suggested `presence`), tunnel-shaped interfaces (suggested `vpn`),
  the routers themselves. DHCP-lease hostnames would make station
  records self-identifying; deferred until bare MACs prove
  insufficient in practice.
- **The remote ASUS router is deferred** — the QuestDB pattern. Its
  reachability today is owner diagnostics (a blackbox probe on the
  monitoring side); an `asuswrt.py` arrives the day its state feeds an
  automation or a family-facing deviation, as a sibling dialect
  adapter, changing nothing here.
- **Dashboard**: `wan = false` and `up = false` join the notable-state
  vocabulary — a downed tunnel is exactly "out of the ordinary".
  Parameters: `poll_interval_s` (owner-editable, default 30) and
  `away_delay_s` ride the live parameter path like any other; both
  have adapter-side fallbacks so a manifest may omit them.

## Virtual sensors: derived state (settled 2026-07-26)

The founding decision: **derived state is ordinary state.** A virtual
sensor — a fused downstairs temperature computed from the room sensors,
the "someone is home" the presence-fusion sentence already promised — is
an ordinary entity with an entity file, whose binding unit is an
automation. "Exactly one adapter binds each entity" generalizes to
**exactly one unit binds each entity**; nothing downstream can tell the
difference, deliberately. Consumers never learn whether a temperature
was measured or fused — the z2m sentence ("adapter-native vocabulary
does not leak onto the bus") applied to provenance. Provenance is still
visible where structure lives: the entity file names its owner, and the
plan renders the automation's bound entities like an adapter's.

- **Mechanics**: `[entities]` becomes legal on automations (optional;
  still required on adapters, still an error on services until one
  needs it). Templated state publishes expand over bound entities
  exactly as for adapters; a single-entity producer may equally declare
  the concrete key. The SDK already covers both (`ctx.publish` +
  entity loading); no SDK change.
- **Everything downstream is free, which is the argument for the entity
  file**: the recorder (subscribes `home/state/**`), the dashboard
  widget (capability + features → value + sparkline), the core state
  mirror and `read_state`, notable-state vocabulary, voice grammar
  later — all generated from the entity registry. A free-form state key
  would be recorded but invisible to every generated surface: hidden
  state outside the repo, exactly what the project rejects.
- **So state keys belong to bound entities, enforced at plan time**: a
  state-class publish must fall under an entity the unit binds —
  templated expressions are bound by construction; concrete ones must
  name a bound entity's room and name literally (`state-publish-unbound`
  otherwise). This closes a pre-existing hole: nothing previously
  stopped a unit from publishing state under an entity it never bound,
  or under no entity at all.
- **Read-only, v1**: automation-owned entities take no commands. A
  cmd-class grant resolving onto one is a plan error
  (`virtual-entity-commanded`), and arbitrated write policy on one is
  likewise refused — write modes govern command writers, and there are
  none. Structural consequence: grant edges still only run adapter →
  dependent, the grant graph stays bipartite, the apply walk cannot
  cycle. State-subscription chains between automations need no
  ordering, as ever — a late-joining consumer reads the mirror. A
  commandable virtual entity (a house-mode switch is the tempting
  case) is the pytapo rule: designed the day one is actually wanted,
  because it brings automation → automation grant edges and cycle
  handling with it. When that day comes, the safe shape is a **latch**
  — commands set the entity's own state, consumers react by
  subscription at their own bands — never a **relay** that re-publishes
  commands onward at the owner's band, laundering the writer's band and
  actor (the group-action settlement under Dashboard is the standing
  example of why).
- **Room**: a cross-room fusion lives in the pseudo-room `global` —
  "downstairs" is a zone, zones never appear in keys, and the existing
  zone-room-collision check already forbids smuggling a zone name in as
  a room. A virtual sensor that is honestly about one room may use that
  room. If `global` placement renders poorly, the escape hatch is the
  dashboard's ordering/pinning hints — never a second spatial truth.
- **Staleness is the producer's obligation**: a fusion of stale inputs
  goes stale rather than confidently republishing — publish on
  transition, one health event per input-loss transition (the openwrt
  failure-policy precedent as a norm for producers). Norm, not
  machinery: the core cannot know which inputs a fusion needs.
- **Rejected, deliberately**: a `derived` key class (fragments the
  vocabulary every consumer keys on); a generic fusion adapter
  configured with rules (fusion config is a DSL — which sensors, what
  weights, is house-specific behavior and lives in the house repo as
  code); cross-adapter fusion inside an adapter (the membrane absorbs
  dialects, it does not compute house behavior — an adapter derives
  freely on its own bound entities, ivt490's normalized readings being
  the standing example, and no further).

## Sensor dropout and availability (settled 2026-07-31)

The founding decision closes the gap between the two liveness layers.
Unit dropout has been solved since step 2 (liveliness tokens, supervisor
health); a device dropping out behind a live adapter was invisible —
state payloads are bare values, the last-value mirror serves them
forever, and a late joiner cannot tell a fresh reading from one whose
sensor died days ago. The crux: **publish-on-transition makes silence
ambiguous.** The bus cannot distinguish "no change" from "no sensor";
only the party with protocol knowledge can — z2m's availability timers,
an ESPHome TCP session, an ONVIF pull-point subscription, a firmware's
known publish cadence. That is dialect knowledge, so it lives in the
adapter — the membrane rule.

- **Availability is ordinary state** — the virtual-sensor sentence
  applied to device liveness. `available` (bool) is a base aspect in
  the schema vocabulary, orthogonal to capability, published on
  transition at `home/state/{room}/{entity}/available` by the entity's
  owning unit. Opt-in, the discovery shape: an adapter with a real
  loss signal publishes it; one with nothing to say (owntracks — a
  retained phone position has no liveness semantics) does not fake
  one. Everything downstream falls out unbuilt: the recorder gives
  per-device availability history, the core state mirror covers late
  joiners, `available = false` joins the notable-state vocabulary (a
  dead sensor is a deviation on `Now`, family-visible exactly like a
  downed tunnel), and automations subscribe to it like any other
  aspect.
- **Per adapter, the loss signal**: z2m maps the bridge's availability
  feature through (`zigbee2mqtt/{id}/availability`, both the
  `{"state": ...}` and legacy bare-string payloads; availability must
  be enabled bridge-side — an operational note, not house config;
  without it the aspect simply never appears, which is the opt-in
  working as designed). esphome flips per device on ReconnectLogic
  connect/disconnect. onvif flips per camera on pull-point
  subscription loss/recreate — the same transitions that already emit
  `event-stream-lost`. ivt490 runs a receive timer against the
  firmware's publish cadence (`availability_timeout_s`,
  owner-editable, adapter-side fallback 300 s — the openwrt
  poll_interval_s shape), emitting one `device-silent` health event
  per down transition. openwrt's own settled failure policy is this
  norm and is unchanged.
- **Stale-not-false graduates from openwrt policy to house-wide
  norm**: on device loss the existing aspect values stand, `available`
  flips, and the adapter never publishes invented values, nulls, or
  clears keys. Unknown ≠ false; one boolean beside the values beats a
  tri-state smeared across every aspect.
- **`available` is reserved vocabulary**: an adapter whose open
  passthrough could mint the aspect from a native field (a z2m field
  name, an ESPHome object_id) drops that field with a
  `reserved-aspect` health event instead of letting a device
  impersonate its own liveness signal. Enumerated dialects (ivt490's
  28 fields) need no runtime guard — a new firmware field arrives
  only by adapter edit.
- **Consumer policy stays in the consumer**: whether a stale input
  means hold, fall back, or go stale downstream is house behavior —
  the fusion argument. The virtual-sensor staleness norm now has
  something mechanical to subscribe to instead of inventing per-input
  timers. Commands toward an unavailable entity likewise stay
  per-adapter (esphome drops with `device-unavailable`, MQTT dialects
  fire into the broker and let the device miss it); a uniform rule is
  the pytapo pattern — built the day something needs it.
- **The consumer helper, shaped ahead of need (2026-07-31), built the
  day the first consumer lands** (the promised presence fusion, by all
  signs). Because `available` publishes on transition only, a
  late-joining subscriber that skips the get runs blind until the next
  transition — possibly weeks away — so the subscribe-then-get-merge
  seed is a correctness trap every consumer would have to hand-roll.
  That mechanical part is the SDK's: `ctx.availability(binding)`
  returns a live per-entity map (seeded via get, updated by
  subscription). The subscription itself is an **explicit
  `[bus.subscribes]` binding** (`home/state/.../available`), never
  implicit — the config-subtree carve-out is about a unit's own
  namespace; watching *other* entities' availability is exactly the
  declared-surface territory the manifest exists to render. Policy
  (hold, fall back, go stale downstream) stays in the automation, as
  above. Likely fellow traveler: the producer-side publish-on-
  transition idiom exists twice (openwrt's `publish()`, ivt490's
  `set_available()`); a fusion publishing its own `available` is the
  third strike that graduates it to an SDK helper too.
- **Rejected, deliberately**: a TTL on the core's last-value cache
  (the core cannot know cadence; silence is ambiguous by
  construction, and a lock is rightly silent for months); timestamps
  as the mechanism (age without cadence knowledge answers "when", not
  "should I trust this" — a transition-published value is supposed to
  be old).
- **The honest limitation, documented**: `available` is device
  liveness, not data freshness. z2m's passive check-in timer for
  battery devices is on the order of hours — a motion sensor dying
  mid-`occupancy = true` stays trusted-and-wrong until the bridge
  notices. An automation needing bounded-age input still needs its own
  cadence (house knowledge: a parameter), never a core TTL. The
  bookkeeping behind it is the SDK's `Freshness` (2026-09-07, #7,
  graduated from a real fusion's private copy rather than waiting for
  the rule of three, because the shape was already settled by use):
  latest value and monotonic seen-time per source, `fresh(max_age_s)`
  at recompute. A live triggering sample is age zero by construction, so
  the fresh set is never empty — the trap is closed once, in the
  helper. It owns no timer: reacting to outright silence is a
  `home/clock/minute` subscription calling the same `fresh()`.
- **A restarted automation catches up from the mirror, with age**
  (2026-09-09, #36). `ctx.subscribe` did not do the subscribe-then-get-
  merge that `Context.__init__` already did for config, so a unit that
  needs every source before it can compute was silent after a restart for
  up to the slowest source's interval — measured at 4 min on a real
  house, 30 min worst case — while every value it needed sat in the
  mirror. With device feeds that silence can outlast the fed device's
  validity window and change how the house is heated. Now `subscribe`
  reads the mirror after declaring its subscribers and delivers what the
  subscription has not already delivered. The mirror's replies carry the
  value's age (seconds since the mirror received it, in the attachment),
  because a catch-up cannot otherwise be told from a fresh publish and a
  six-hour-old reading fed into a moving average as new would trade one
  silent failure for another: a handler that takes `age_s` hands it to
  `Freshness.seen`, and a catch-up older than the policy drops out of the
  first recompute. The one consequence for such a handler: a catch-up can
  leave the fresh set empty, so it checks. Age rather than a wall-clock
  stamp because only the mirror's clock is involved and the reply is read
  the moment it is made; a two-argument handler keeps working and treats
  the catch-up as just-arrived, which is still strictly better than
  blindness.
- **Availability must be able to say "no information".** A backend
  configured without availability at all (z2m with no `availability:`
  block publishes no such topics) yields an empty map that reads as
  everything-is-up. When `ctx.availability()` is built it returns three
  states per entity — up, down, unknown — and unknown is the honest
  answer for an entity whose adapter never published `available`
  (recorded 2026-09-07 from #7, ahead of the helper).

## One-way senders: who synthesizes the off (settled 2026-09-11, #63)

A sub-GHz PIR, door contact or smoke detector transmits when something
happens and never transmits again — there is no "clear". The same is
true of doorbells, RF remotes and most cheap 433 MHz kit. Something has
to decide when the assertion stops being true.

**The adapter owns the hold.** Three existing settlements decide it: the
membrane rule (deriving on its own bound entities is what an adapter
may do, and the radio's lack of an off is a protocol fact), the
availability settlement's "never a core TTL" (a core-decayed momentary
aspect is that timer under another name), and the pytapo rule (core
machinery is designed the day a second case wants it).

- **Transitions only.** `true` on the first assertion, `false` when the
  hold expires; a repeat burst inside the hold extends the deadline and
  publishes nothing. One-way senders repeat each burst by design, so
  publishing per burst is a per-event flood.
- **`false` for every bound momentary entity at startup.** The held
  value is the adapter's own construct, not a device reading, so
  "nothing has asserted within the hold" is the honest state after a
  restart rather than an invented one — and it is what stops a
  crash-looping adapter from leaving a motion sensor stuck on. Only a
  permanently dead adapter (breaker open) leaves `true` standing, the
  same accepted limitation as a dead Zigbee bridge, and the unit's
  health shows it.
- **The hold is a per-aspect parameter, not per entity.** A contact, a
  PIR and a detector want different holds; every contact wants the same
  one. The entity's capability and features pick the parameter, so a
  house tunes three numbers rather than one per device — and entity
  files keep denying unknown fields.
- **Revisit trigger: the rule of three.** A second one-way adapter
  carrying the same timer graduates it into the SDK, the way
  `Freshness` and `Cooldown` did. Still not into the core.

**Rejected**: a momentary aspect class decayed by the core (the TTL the
availability settlement already refused); consumer-side debouncing
(every consumer reimplements it, they disagree, and the recorder cannot
reconstruct what was true when).

## Unit granularity: the atom is the unit, not the automation (settled 2026-09-08)

The question was whether every automation, however small, should be its
own `uv`-run process. The answer is that the question conflates two
things. **The unit is the atom**: of authority (its manifest's grants,
subscribes, publishes, params), of failure (its liveliness token, backoff,
breaker, process group), and of change (its `files_hash`, its step in the
apply walk). Those three boundaries coinciding on one process is the
architectural payoff of the process model, and nothing here moves any of
them. **What a unit contains is the author's call.** A unit script may
host several rules — several `ctx.subscribe` handlers, a minute-tick
handler, a fused sensor — and the SDK already supports it: the Context is
callback-driven with no limit on subscriptions, and the manifest expresses
the union of what the rules need. No SDK or schema change; this is a
statement of what was always legal.

- **The grouping rule is shared blast radius, not size.** Rules belong in
  one unit when they should live and die together: one throws on the next
  tick and the others going down with it is acceptable, they share a
  restart, they share `home/health/{unit}`, and an edit to any of them is a
  behavioral change to all of them at plan time. "All the evening
  lighting" is one unit; "evening lighting" and "the heat-pump setback" are
  two, however small each is, because nobody wants a bug in one to restart
  the other.
- **Authority is the union, deliberately.** A unit that hosts three rules
  holds the grants all three need, and any of them can use any of them —
  the grant table cannot tell rules apart, only units. That is the cost of
  bundling and the reason the boundary stays at the process: a rule that
  must not be able to touch what its neighbour touches is a separate unit.
- **What a unit costs** (measured 2026-09-08, dev container, warm): a
  minimal Python unit — zenoh and the SDK imported, nothing else — is
  ~12 MB resident. That is the floor per unit now that the `uv run` parent
  is gone (see Supervision); a heavy adapter is more, a trivial automation
  is not less. Forty trivial units is ~0.5 GB, fine on a NUC or a Pi 4,
  not on a Pi Zero. The lever if that ever binds is bundling by the rule
  above, not a shared runner.
- **Rejected: a multi-tenant automation runner** — one service hosting
  many small rules with an in-process scheduler. It reintroduces shared
  authority and shared failure across rules that did not choose it, and
  the moment it grows per-rule health and restart it is the supervisor
  rebuilt in Python. That is the Home Assistant model the project set out
  to leave.

## Burners and interlocks (settled 2026-09-09, #37, #38)

A house with a heat pump and a pellet burner has two heat sources, and an
automation that chooses between them needs both describable in the same
terms. #37 proposed a `burner` capability rather than `switch` plus loose
sensors; #38 asked where the burner's flue-temperature cutout should stand
in the arbiter. Both settled here.

- **Vocabulary: `burner`**, deliberately small. Base aspect `on` (bool, the
  family lever, the burner analogue of `setpoint` and `locked`); feature
  `power_level` (the output setting as the device enumerates it — the
  reporting device offers 10/50/100 — described as a constraint so the
  existing enum → segmented-control path renders it); normalised readings
  `flue_temperature` and `boiler_temperature` in °C. Everything else passes
  through under its firmware name, as `ivt490` does.
- **Why a capability and not switch + sensors.** Modelled as a `switch` a
  burner is a boolean with no meaning attached: "is it making heat" would
  have to be read from dialect fields, which is what the vocabulary exists
  to prevent. And per-aspect leases need `on` and `power_level` to be
  aspects of one entity, so a family member holding the burner off does not
  freeze an automation's power-level choice — the same reason the heat pump
  moved to per-aspect leases.
- **`power_level` is vocabulary for a safety reason** as well as a UI one:
  a flue-temperature cutout's threshold is a function of the power level,
  so an interlock needs both as first-class inputs. The test that fell out,
  worth keeping: *the inputs an interlock needs are vocabulary; the
  thresholds are configuration.*
- **`on` reads back from the device.** In the reporting dialect start and
  stop are momentary writes and the true on/off is derived from the run
  state, so `on` must never be an echo of the command — the discipline
  `ivt490` already states for `setpoint`.
- **No run-phase vocabulary yet.** A `state` of `off / igniting / running /
  cleaning / fault` is what would make an automation portable across
  burners, but no code table exists anywhere in the reporting chain and
  after ten idle days exactly one code has ever been observed. A phase
  vocabulary now would be invented rather than generalised. `state` and
  `substate` pass through raw; revisit after a heating season, ideally
  against a second burner adapter.
- **Not `heat_source`.** The arbitration argument is really about heat
  sources, not combustion, but an abstraction spanning a burner and a heat
  pump that is already `climate` would be built against one example — the
  same objection. Grow it when a second case demands it.
- **No dashboard widget in the same change.** The described-card fallback
  already renders any capability without a bespoke widget from its
  descriptors (`climate` uses it); `burner` gets a card the day someone
  wants one.
- **Cost note, for whoever writes the adapter.** The reporting bridge
  republishes all 30 topics every ~32 s whether or not anything changed,
  and its `status` topic alone is 116 fields — naively that one topic is
  ~310k recorder rows/day, two and a half times the entire `ivt490`
  adapter, which is already 94% of that house's store. Publish on change
  (the source is republish-on-poll, so forwarding inherits the full poll
  rate for values that never move — #3 one layer out), and treat the
  `settings/*` topics as configuration, not samples.

### Aduro adapter (built 2026-09-09)

`adapters/aduro.py`, against the reporter's `aduro2mqtt` bridge (NBE UDP
to MQTT). The entity `id` is the bridge's base topic; one entity per
burner; arbitrated, so `on` and `power_level` lease independently.

- **Publish on change, from two topics.** Only `{base}/status` and
  `{base}/operating` are subscribed; a field publishes when its value
  differs from the last one put on the bus (and once after start). The
  identical republish a poll later yields nothing. Settings, consumption,
  advanced and logs are not subscribed at all. Status fields keep their
  firmware names, dots included; operating fields carry an `operating_`
  prefix so the two NBE namespaces stay apart without a table.
- **`on` is derived**, from `state` not being in a set of off codes. The
  set holds the one code observed (14, idle and unlit); it grows from the
  heating season, which is why `state` and `substate` pass through raw
  beside it. `power_level` is `regulation.fixed_power` as an int;
  `flue_temperature` is `smoke_temp`; `boiler_temperature` is
  `boiler_temp`. `shaft_temp`, the device's own fire-safety reading, passes
  through labelled.
- **Commands** are the bridge's own `{path, value}` shape on `{base}/set`:
  a bool `on` becomes a momentary `misc.start` or `misc.stop`; an integer
  `power_level` in {10, 50, 100} becomes `regulation.fixed_power`. Anything
  else drops with `invalid-command`. Both are family-tier in the
  descriptor, `on` described as a two-valued enum so the described card
  gets a segmented control without a bespoke widget.
- **Availability** is the receive timer (`availability_timeout_s`,
  default 300 s, about nine polls): the bridge skips a topic when the
  burner does not answer, so an unreachable burner and a dead bridge both
  go silent.

### Interlocks stay the device's job (settled 2026-09-09, #38)

The house runs two flue-temperature cutouts (stop above 200 °C at 10%
power, above 225 °C at 50%) and wanted to port them as a house-local unit.
No band fits: at `automation` the interlock and the 60 s heating loop it
guards against sit at the same band, and equal-or-higher passes and takes
the hold, so it is defeated within a minute; at `manual` it works and lies,
attributing a thermal cutout to the family in every audit surface. Deeper,
the arbiter's hold is *timed* and an interlock is *conditional*: the burner
should stay stopped while the flue is hot, not for `hold_minutes`. Refreshing
the hold on every sample turns the interlock into a continuous writer whose
death silently releases the burner to the automation.

- **Decision: homeostat is not in the safety path.** The device carries its
  own alarm layer (the Aduro's `max_shaft_temp`, `min_boiler_temp`), and
  that is where combustion safety lives. A house-local cutout that
  additionally publishes `on = false` at the automation band is welcome as
  belt-and-braces, but it is an automation like any other — contestable,
  timed, honest about its band — and the house must not rely on it. This
  is written down so "no band for interlocks" reads as a decision rather
  than as "not yet".
- **Rejected: a fifth band above `manual`.** It contradicts THE FAMILY
  ALWAYS WINS OVER AUTOMATIONS by adding a constant, when whether safety
  outranks the family is a values decision that deserves to be made in the
  open, and it inherits the timed hold that is the wrong shape anyway.
- **Deferred, not rejected: an inhibit class.** A unit asserting a lockout
  on `(entity, aspect)` while a condition holds, the arbiter refusing every
  band for as long as it is asserted, assertion and release published as
  events — condition-based, honest about the actor, visible. It is the
  right shape (an interlock removes an option; it does not want to win an
  argument — the same distinction #9 drew between a feed and a command),
  and it generalises to alarm-armed locks, dry-run pumps, valves held for
  maintenance. It is also machinery, and it is being deferred against one
  case. If interlocks recur, this is the design to pick up.

## Notifications (settled 2026-09-09, #31)

A unit could publish state, commands, health and config, and none of it
reached a person. #31 surveyed the reporting house's Node-RED estate and
found three of five automation groups (intrusion, irrigation, heating)
unportable without a way to tell someone, and asked whether that is in
scope at all, what shape it takes, and how it is gated.

**In scope, and it is a capability delivered by an adapter.** Reaching
a person is reaching a device the house binds: a phone's notification
channel has a dialect exactly as a lamp does, and the adapter that speaks
it embodies the channel as an entity. `notifier` is a capability; an
entity file per addressee binds it to a delivery adapter; an automation
that wants to reach one declares an ordinary cmd-class publish, granted
at plan time onto that entity. Nothing new in the core beyond the
vocabulary row.

- **Vocabulary.** Base aspect `message` (string, commandable); feature
  `alert` (string, commandable). Severity is an ASPECT, not a field in
  the payload, so the two classes are structurally separate delivery
  paths: separately grantable (`home/cmd/person/*/alert` grants alerts
  and nothing else), separately policed by the adapter later (quiet
  hours withhold `message`, never `alert`), and separately rendered in
  history. The payload is the message itself, a bare string, the scalar
  the recorder stores natively. Nothing dialect-shaped enters the
  vocabulary: chat ids, topics, priorities, parse modes and receipt
  semantics are the adapter's, in its credentials and its code.
- **Addressing is the entity.** One entity per channel, pseudo-room
  `person` for a person's phone, `global` for a group. A group is either
  a fan-out loop in the automation (the group-actions settlement:
  fan-out at the edge, never a relay) or a group channel bound as its
  own entity, whichever the dialect makes honest. A person with two
  channels is two entities; switching providers is a plan/apply
  migration, and the automations do not change. Nothing "person has
  channels" exists in the model, deliberately.
- **Gating is the grant table, unchanged.** Adding a `notifier` publish
  to a manifest is a grant delta, so the plan is structural and lands as
  a pending plan for the owner; the smuggling criterion of the agent
  surface already covers an agent trying it. The plan renders who may
  reach whom; a mistyped entity is the existing "matches no entities"
  warning; the SDK refuses a key outside the declared expression. The
  issue's worry that "the grant table would describe who may speak" is
  the point: the table already says which unit may do what to which
  entity, and a phone is one more entity a compromised automation can
  do harm through.
- **The envelope's band is inert.** Channels are `shared`, so no lease,
  no preemption, no arbiter. Every cmd payload still carries
  `{value, priority, actor}`; `actor` is exactly what the audit wants
  (the recorder stores every message with who sent it and when, and
  `home/history/events` answers "what was sent to Alice yesterday"),
  and `priority` carries no meaning here. Written down rather than
  reinterpreted.
- **Rate limiting splits in two.** The cooldown itself is house policy
  (the reporting estate's one-per-ten-minutes intrusion limiter is
  load-bearing against the 417-samples-in-56-seconds motion episode of
  #3) and lives in the automation as a family-editable parameter; the
  bookkeeping is the SDK's `Cooldown` (graduated on the Freshness
  argument: the shape was settled by a live limiter, not invented). The
  adapter carries a per-entity floor, `min_interval_s` (owner-editable),
  as defense in depth — the ivt490 bounds argument — dropping with a
  `rate-limited` event. A runaway automation still writes every attempt
  as a cmd row, so the noise costs something visible, which is the
  retention settlement's pressure back toward the producer.
- **Failure is loud by existing machinery.** The adapter verifies its
  server before `ready()`, so a bad token or an unreachable server is a
  startup error the supervisor's backoff shows. Every undelivered
  message is a `drop` with reason `delivery-failed`. The channel's
  `available` flips false on a failed delivery and true on the next
  success — notable-state vocabulary, so a dead channel is a deviation
  on `Now`, recorded, and subscribable. The adapter publishes
  `delivered`, the epoch time the server acknowledged the last message
  (the `fixed_at` shape), so history holds the sent row and the acked
  row side by side. The honest limit: `delivered` means the delivery
  service accepted the message, not that a human saw it. A true far-end
  acknowledgment arrives with a homeostat app (below).
- **Rejected, deliberately.** An external subscriber (cannot carry
  intent: "irrigation skipped because it rained" is not derivable from
  state, and if it is a unit it is in scope anyway). Health events as
  the channel (no addressee; every event becomes a candidate). A new
  class `home/notify/**` with a notifier service (the addressee becomes
  hidden structure — a name in an out-of-repo file the plan cannot
  check, so a typo goes nowhere silently; gating needs a new grant
  kind, which the feeds settlement already refused as machinery;
  nothing downstream is free; a new class fragments the vocabulary
  consumers key on, the `derived`-class objection). An SDK facility
  (authority by import). Notification as state plus a routing service
  (the routing table is a rules DSL in a manifest, the generic-fusion
  rejection; an alarm condition may ALSO deserve a virtual entity so
  `Now` shows it, which is orthogonal to delivery). Dashboard web push
  (no secure context, and "looking at the dashboard" is what the issue
  excludes).

### The ntfy adapter (built 2026-09-09)

The first delivery dialect. Signal and WhatsApp were ruled out for the
reporting house because both need a phone number for the house's
identity (Signal through signal-cli, a foreign binary on the go2rtc
shim pattern; WhatsApp through the Business Platform, or through your
own account, which makes every message come from you). Telegram needs
none and was the runner-up. ntfy won on fit: a small self-hostable push
server with an Android app, no account, no number, no third party when
self-hosted, local-only over WireGuard exactly like the dashboard, and
it speaks UnifiedPush, which is the push transport a homeostat app would
use — so this adapter is not thrown away when the app arrives.

- **ntfy is a compose sidecar, not a unit.** The phones connect to it
  directly, so it is the peer of the MQTT broker, not of go2rtc, and its
  lifetime must not follow a unit restart. Homeostat only publishes to
  it. `adapters/ntfy.py` is a plain Python unit; no foreign binary in
  the image.
- **Configuration is text, all of it.** ntfy provisions users, access
  rules and tokens from its config file (`auth-users`, `auth-access`,
  `auth-tokens`, ntfy ≥ 2.12; re-applied on every start, removed when
  they leave the file). The house repo carries `ntfy/server.yml` with
  the base URL, cache window and the access rules — topics are the
  notifier entity ids, the publisher writes only, each person reads
  only their own topic and the group's — and the env file beside the
  compose file carries the bcrypt user hashes and the token. Adding a
  family member is an entity file, two access lines and a user entry.
  The access list duplicates what the entity files say (the z2m
  base-topic shape); rendering it from the entity files would make
  ntfy a shim unit, and the sidecar's independent lifetime is worth
  more than three lines.
- **Binding.** The entity `id` is the ntfy topic. `[discovery].endpoint`
  is the server URL (compose-internal, not a secret); the publisher
  token is `HOMEOSTAT_NTFY_TOKEN` in the environment, never in the
  repo. Startup GETs `/v1/health` before `ready()`.
- **Mapping.** `message` → POST `{endpoint}/{topic}` with the string as
  the body at ntfy priority 3, the actor's unit name as the title;
  `alert` → the same at priority 5, which the Android app treats as
  urgent: it overrides Do Not Disturb and plays a continuous alarm
  tone. The severity split therefore maps onto something the phone
  enforces. The server's reply carries the message id and time;
  `delivered` is that time. A non-2xx or a connection error drops with
  `delivery-failed` and flips `available`.
- **What the cache window means.** The phone's app fetches what it
  missed on reconnect, back to `cache-duration`; a message older than
  that when the phone returns is lost. A phone must reach the server
  from wherever it is — for the intrusion flow, which fires while the
  house is empty, that means WireGuard always-on on the family phones
  — or the alert waits in the cache until it does.
- **The homeostat app is the designated growth path** (the Frigate
  pattern): one more adapter binding its own `notifier` entities,
  acknowledging for itself, changing zero automations.

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
- SDK distribution (settled 2026-07-05, REVISED 2026-08-29): a house unit
  names `homeostat==X.Y.Z` in its PEP 723 dependencies and carries NO
  `[tool.uv.sources]` block. The image bundles the SDK wheel and sets
  `UV_FIND_LINKS`, so the unit resolves it locally — no clone at first
  boot, no network for the SDK, and the pin is still a version in the unit
  script that `files_hash` covers, which is the property the original
  settlement was for. The git source it replaces cost an order of
  magnitude in memory (38.4 MB against 4.0 MB in the long-lived `uv run`
  parent, see Supervision); a version pin was already the anticipated end
  state here ("PyPI publication later keeps the same shape"), and it
  resolves from PyPI unchanged if that ever happens. What it costs: a
  house pinning a version the running image does not bundle fails to
  resolve at unit start — the version-floor hazard, in its loudest and
  most diagnosable form. In-repo `adapters/` keep a relative `path`
  source, so tests still exercise the working-tree SDK; never a vendored
  copy. The pin lives in the unit script, which
  files_hash covers, so an SDK bump is a visible behavioral change to
  plan/apply; a vendored copy sits outside change detection and was
  rejected for exactly that reason. In-repo adapters and fixtures keep
  relative `path` sources so tests exercise the working-tree SDK. PyPI
  publication later keeps the same shape (`homeostat==X.Y.Z`).

  Measured 2026-08-29, and acted on, recorded as a finding and NOT a decision: the
  source form dominates a unit's resident memory. Same heavy environment
  (aioesphomeapi + zeroconf), warm, uv 0.9 in the release image — git
  source 38.4 MB in the long-lived `uv run` parent, a built wheel 4.0 MB,
  a path source 4.0 MB. Four heavy units on a live house is ~136 MB of the
  223 MB measured in parents there. Publishing the SDK would therefore buy
  an order of magnitude more than the supervisor's prewarm did. What it
  costs is the property this settlement was chosen FOR — the pin lives in
  the unit script, `files_hash` covers it, and an SDK bump is a visible
  behavioral change to plan/apply. A wheel pinned by version keeps that;
  a wheel pinned by path or floated does not — which is why the revision
  above pins by VERSION and bundles the wheel rather than naming a path.

  The trap:
  "pin by git source" reads as "pin to a release", but an adapter copied
  from main against the newest *tag's* SDK raises AttributeError on
  whatever the SDK has grown since — adapter and SDK must come from the
  same commit, so a house vendoring ahead of a release pins `rev`, not
  `tag`. `examples/starter-house` avoids the question by being a snapshot
  of the release it pins, regenerated by `scripts/sync_starter.sh` and
  held there by CI.
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
6. Agent MCP surface (goal and settlements above, under "Agent
   surface"). DONE.
7. Dashboard (design settled above; wireframes in `docs/wireframes/`).
   MVP DONE.
8. Voice. Deferred — not yet begun.
Risk lives in steps 1 and 2; everything after is accretion.
 
## Open questions (flagged, not settled)
 
- Whether `features` should gate command contents beyond SDK constructors.
  Current lean: no separate layer.
- Zenoh ACL hardening timeline.