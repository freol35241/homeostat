# The starter house, with simulated devices

Everything the starter house runs — the clock, the recorder, the arbiter,
the dashboard, the evening-lights automation and the Zigbee2MQTT, IVT490,
OwnTracks and ESPHome adapters — running unchanged, against a simulator
that stands where the hardware would be. No coordinator stick, heat pump,
phone or ESPHome board needed.

## Start it

In a browser, with nothing installed: open the repository in GitHub
Codespaces with the demo configuration (the "Open in Codespaces" button
in the top-level README). The dashboard opens by itself once the house
is up; the first boot takes a couple of minutes while every unit
resolves its environment.

On your own machine, with Docker:

```
examples/starter-house/demo/up.sh        # copies the house to ~/homeostat-demo and starts it
open http://localhost:8600               # the dashboard
examples/starter-house/demo/up.sh logs -f homeostat   # watch the units start
examples/starter-house/demo/up.sh down   # stop it
```

`DEMO_DIR` chooses where the copy lives. It is a real house repo, so
everything in the starter's README applies to it: `plan` and `apply`
through `docker compose exec`, the recorder's store in `data/`.

## What is simulated

`simulator.py` speaks the far side of each protocol, so the adapters
cannot tell it from the real thing:

| Device | Protocol | Behaviour |
|---|---|---|
| living-room lamp | Zigbee2MQTT | obeys on/off and brightness; someone turns it on now and then |
| front-door lock | Zigbee2MQTT | obeys lock/unlock (through the arbiter); someone comes in every so often |
| hallway motion | Zigbee2MQTT | someone walks through every few minutes |
| heat pump | IVT490 board over MQTT | outdoor temperature on a daily curve; indoor drifts toward the setpoint in about ten minutes |
| Alice's phone | OwnTracks | home for five minutes, then a ten-minute walk around the block |
| porch switch | ESPHome native API | obeys on/off |

`SIM_HOME="lat,lon"` moves the house (Stockholm by default).

## How it is wired

`docker-compose.demo.yml` is layered over the house's own compose file:
it parks `zigbee2mqtt` behind a `hardware` profile, adds the `simulator`
service, and tells the ESPHome adapter where the porch switch is
(`esphome-devices.toml`, since an mDNS name does not cross the compose
network). Nothing else in the house changes. Delete `demo/` when you move
in with real devices.
