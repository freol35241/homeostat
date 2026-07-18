# Starter house

A runnable house-repo template: clock, recorder, a Zigbee2MQTT adapter,
and the evening-lights automation, with a compose file for the whole
stack. Units pin the Python SDK from a released tag, so this directory
is self-contained — copy it out and make it your own repo:

```
cp -r examples/starter-house ~/house && cd ~/house
git init && git add -A && git commit -m "day one"
```

## Try it without hardware

Runs everything except the Zigbee coordinator; the adapter connects to
mosquitto and idles:

```
docker compose up -d mosquitto homeostat
docker compose logs -f homeostat   # watch units go starting -> running
```

## Real devices

You need a Zigbee coordinator stick (e.g. SLZB-06 or Sonoff ZBDongle-E).

1. Point `devices:` and `ZIGBEE2MQTT_CONFIG_SERIAL_PORT` in
   `docker-compose.yml` at your stick, then `docker compose up -d`.
2. Pair devices through the Zigbee2MQTT frontend at `:8080` and give
   them friendly names.
3. For each device, write an entity file under `entities/zigbee/` —
   `id` is the friendly name; the file stem is the entity name on the
   bus. Add its room to `zones.toml` if new. The two entity files here
   are examples: replace them with your devices.
4. Commit, then `docker compose restart homeostat` (structural changes
   need a fresh `up`; parameter and behavioral edits flow through
   `plan`/`apply` with no restart).

State appears at `home/state/{room}/{entity}/{aspect}`, history lands in
`data/history.db`, and the automation turns downstairs lights off after
`off_time` (a live-editable parameter) when nobody is present.

## The family dashboard

`units/dashboard.toml` serves the web dashboard on `:8600` — generated
entirely from the manifests: rooms and their devices, every
family-editable setpoint, unit health, and a "Now" view showing what
deviates from normal. Open `http://<host>:8600` from the LAN (or over
WireGuard). Access is local-only by design: there are no accounts; the
dashboard is family-tier, so nothing structural is reachable from it.
Serving it behind a hostname other than `homeostat.lan`/`.local`? List
it in the `HOMEOSTAT_DASHBOARD_HOSTS` environment variable
(comma-separated) on the homeostat service.

Already running Zigbee2MQTT elsewhere? Delete the `mosquitto` and
`zigbee2mqtt` services from the compose file and point the `endpoint`
in `units/zigbee.toml` at your existing MQTT broker instead.

## Devices and phones on the LAN

The broker has two listeners (`mosquitto.conf`): 1883 stays inside the
compose network — anonymous, for zigbee2mqtt and the adapters — and
1884 is published to the LAN for MQTT clients that live outside the
stack: the IVT490 heat-pump interface (an ESP8266 can't join a VPN)
and OwnTracks phones. 1884 requires credentials, and `mosquitto.acl`
confines each one to its own topic tree, so a leaked device password
can touch that device's dialect and nothing else — never
`zigbee2mqtt/#`.

The template ships `mosquitto.passwd` empty (nobody can connect) and
gitignored — password hashes are credentials and never belong in the
house repo. Add a user (a throwaway container, because the running
broker mounts the file read-only), then restart the broker:

```
docker run --rm -it -v ./mosquitto.passwd:/passwd eclipse-mosquitto:2 \
  mosquitto_passwd /passwd ivt490
chmod 600 mosquitto.passwd
docker compose restart mosquitto
```

Point the heat-pump firmware's `MQTT_HOST`/`MQTT_PORT`/`MQTT_USER`/
`MQTT_PW` and each phone's OwnTracks connection at `<host>:1884`, one
user per client, and mirror every new user in `mosquitto.acl`.

Plan and apply from the host, against the running house:

```
docker compose exec homeostat homeostat plan /house --bus tcp/127.0.0.1:7447
```

## Let an agent configure it

The house runs its own agent surface: `units/mcp.toml` serves MCP over
HTTP on `:8642`. Connect any MCP client, e.g.:

```
claude mcp add --transport http homeostat http://<host>:8642
```

The agent gets five tools — `read_state`, `read_history`, `plan`,
`propose`, `apply` — with tier-gated authority: it can read everything,
and `propose` commits repo edits, but only parameter-only changes apply
immediately. Anything behavioral or structural (new entities, new
units) is saved as a pending plan under `plans/pending/` for you to
review and apply:

```
docker compose exec homeostat homeostat apply /house \
  --bus tcp/127.0.0.1:7447 --plan plans/pending/<id>.plan
```

Discovery closes the loop: the zigbee adapter republishes the bridge's
device inventory at `home/discovery/zigbee` — every paired device with
its binding `id`, whether an entity file claims it (`configured`), a
suggested capability stanza mapped from the device's z2m `exposes`, and
the raw definition. So the prompt an agent can act on end to end is:

> Read `home/discovery/zigbee` and propose entity files under
> `entities/zigbee/` for every unconfigured device, using the suggested
> capabilities. Ask me which room each device is in.

Rooms are the one thing no protocol knows — expect the agent to ask,
or correct its guesses when you review the pending plan.
