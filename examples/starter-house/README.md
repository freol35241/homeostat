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

Plan and apply from the host, against the running house:

```
docker compose exec homeostat homeostat plan /house --bus tcp/127.0.0.1:7447
```
