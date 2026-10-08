# Starter house

A runnable house-repo template: clock, recorder, a Zigbee2MQTT adapter,
and the evening-lights automation, with a compose file for the whole
stack. Units pin the Python SDK from a released tag, so this directory
is self-contained. Copy it out and make it your own repo:

```
cp -r examples/starter-house ~/house && cd ~/house
git init && git add -A && git commit -m "day one"
```

Before the first start, create the two files the compose stack reads
but the repo must never contain:

```
cp mosquitto.passwd.example mosquitto.passwd   # broker credentials, empty to begin with
echo "Z2M_FRONTEND_TOKEN=$(openssl rand -hex 16)" > .env   # Zigbee2MQTT frontend login
```

Both are gitignored. The homeostat container runs as uid 1000. If your
checkout is owned by another user, add `HOMEOSTAT_UID=$(id -u)` and
`HOMEOSTAT_GID=$(id -g)` to `.env` so units can write `data/` and
`plans/`. If you are upgrading a house that ran an older image as root,
its `uv-cache` volume is owned by root. Remove it once with
`docker volume rm <project>_uv-cache`; it is only a cache.

## Try it without hardware

`demo/up.sh` runs the whole house against simulated devices: a lamp, a
lock, a motion sensor, the heat pump, a phone and an ESPHome switch. It
serves the dashboard at http://localhost:8600 (see `demo/README.md`).

Or run everything except the Zigbee coordinator, with no devices. The
adapter connects to mosquitto and idles:

```
docker compose up -d mosquitto homeostat
docker compose logs -f homeostat   # watch units go starting -> running
```

## Real devices

You need a Zigbee coordinator stick (e.g. SLZB-06 or Sonoff ZBDongle-E).

1. Point `devices:` and `ZIGBEE2MQTT_CONFIG_SERIAL_PORT` in
   `docker-compose.yml` at your stick, then `docker compose up -d`.
2. Pair devices through the Zigbee2MQTT frontend and give them friendly
   names. It listens on the host's loopback only (`127.0.0.1:8080`) and
   asks for the `Z2M_FRONTEND_TOKEN` from `.env`: open it on the host,
   or from your laptop through `ssh -L 8080:127.0.0.1:8080 <host>`. It
   can pair, rename and reconfigure every device, which is why it is
   not on the LAN.
3. For each device, write an entity file under `entities/zigbee/`.
   `id` is the friendly name, and the file stem is the entity name on the
   bus. Add its room to `zones.toml` if it is new. The two entity files
   here are examples. Replace them with your devices.
4. Commit, then `docker compose restart homeostat` (structural changes
   need a fresh `up`; parameter and behavioral edits flow through
   `plan`/`apply` with no restart).

State appears at `home/state/{room}/{entity}/{aspect}`, history lands in
`data/history.db`, and the automation turns downstairs lights off after
`off_time` (a live-editable parameter) when nobody is present.

## The family dashboard

`units/dashboard.toml` serves the web dashboard on `:8600`. The
dashboard is generated from the manifests. `dashboard.toml` at the house
root says which views it has. Here that is a "Now" view of the indoor
temperature, the people and what deviates from normal, a "Heating" view,
and a "Downstairs" view led by the evening-lights automation's card. If
you delete the file, the generated views (Now, Setpoints, Rooms) come
back. Health, and anything the file does not place, are always one tap
away in the rail. `homeostat plan` refuses a view that names something
the house does not have. Open `http://<host>:8600` from the LAN or over
WireGuard. Access is local-only and there are no accounts. The dashboard
is family-tier, so nothing structural is reachable from it.
Serving it behind a hostname other than `homeostat.lan`/`.local`? List
it in the `HOMEOSTAT_DASHBOARD_HOSTS` environment variable
(comma-separated) on the homeostat service.

Already running Zigbee2MQTT elsewhere? Delete the `mosquitto` and
`zigbee2mqtt` services from the compose file and point the `endpoint`
in `units/zigbee.toml` at your existing MQTT broker instead.

## Devices and phones on the LAN

The broker has two listeners (`mosquitto.conf`). 1883 stays inside the
compose network and is anonymous, for zigbee2mqtt and the adapters. 1884
is published to the LAN for MQTT clients outside the stack: the IVT490
heat-pump interface (an ESP8266 can't join a VPN) and OwnTracks phones.
1884 requires credentials, and `mosquitto.acl` confines each user to its
own topic tree. A leaked device password can only reach that device's
topics, and never `zigbee2mqtt/#`.

The template ships `mosquitto.passwd.example` empty. Your copy,
`mosquitto.passwd` (see the top of this file), starts empty too, so
nobody can connect. It is gitignored, because password hashes are
credentials and do not belong in the house repo. The same goes for the
`.env` that holds the Zigbee2MQTT frontend token. Add a user with a
throwaway container, because the running broker mounts the file
read-only. Then restart the broker:

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
claude mcp add --transport http homeostat http://<host>:8642 \
  --header "X-Homeostat: 1"
```

The header is required. Reachability is this surface's only credential,
and it serves everything the house knows. Without the header check, a
web page open in a family browser could send requests to it at your LAN
address, even though the page could not read the replies. Set
`HOMEOSTAT_MCP_HOSTS` if you reach the house by a name other than
`homeostat`/`homeostat.lan`/`homeostat.local`.

The surface is read-only: `read_state`, `read_history`, `read_logs`,
`read_events`, plus `schema` and `explain` for the authoring contract.
`AGENTS.md` (which `CLAUDE.md` points at) tells an agent working in the
checkout how this house is laid out and where its authority ends.
Changing the house is a repo edit like any other. An agent working in
your house checkout (Claude Code, say) edits files and runs
`homeostat plan`. You review and apply:

```
docker compose exec homeostat homeostat apply /house --bus tcp/127.0.0.1:7447
```

The zigbee adapter republishes the bridge's device inventory at
`home/discovery/zigbee`. Each paired device is listed with its binding
`id`, whether an entity file claims it (`configured`), a suggested
capability stanza mapped from the device's z2m `exposes`, and the raw
definition. An agent can therefore act on a prompt like this from start
to finish:

> Read `home/discovery/zigbee` and write entity files under
> `entities/zigbee/` for every unconfigured device, using the suggested
> capabilities. Ask me which room each device is in, then run
> `homeostat plan`.

No protocol reports which room a device is in. Expect the agent to ask,
or correct its guesses when you review the plan.
