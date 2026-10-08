# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "paho-mqtt>=2,<3",
#     "aioesphomeapi>=45,<46",
# ]
# ///
"""Simulated devices for the starter house's demo.

The simulator plays the device side of every protocol the house speaks,
so the real adapters run unchanged against something that behaves like a
home.

  - Zigbee2MQTT (base topic `zigbee2mqtt`): the retained bridge inventory
    and bridge state, a dimmable living-room lamp, the front-door lock and
    the hallway motion sensor. Commands on `{name}/set` are obeyed as z2m
    obeys them. States are retained, as for a z2m device with
    `retain: true`.
  - The IVT490 heat-pump board (base topic `ivt490`): outdoor temperature
    on a daily curve, a feed temperature that follows the heating demand,
    and an indoor temperature that drifts toward the setpoint. Turning the
    dial shows a response within minutes.
  - OwnTracks (`owntracks/alice/phone`): Alice is home, then takes a walk
    around the block, then comes home again, on a short cycle so the map
    moves while someone is watching.
  - ESPHome (native API on port 6053, device `porch`): the porch switch.

A household lives in it too. Someone walks through the hallway every few
minutes, turns the living-room lamp on now and then, and unlocks the front
door for a minute. The house's own automations respond as they would to
real devices.

Nothing here is part of homeostat; it stands in for the hardware.
SIM_MQTT_HOST/SIM_MQTT_PORT name the broker (default mosquitto:1883, the
compose network's anonymous listener). SIM_HOME="lat,lon" sets where the
house is, and SIM_SEED fixes the random seed.
"""

import asyncio
import json
import math
import os
import random
import time

import paho.mqtt.client as mqtt
from aioesphomeapi import api_pb2

MQTT_HOST = os.environ.get("SIM_MQTT_HOST", "mosquitto")
MQTT_PORT = int(os.environ.get("SIM_MQTT_PORT", "1883"))
HOME = tuple(float(x) for x in os.environ.get("SIM_HOME", "59.3326,18.0649").split(","))
ESPHOME_PORT = int(os.environ.get("SIM_ESPHOME_PORT", "6053"))
RNG = random.Random(os.environ.get("SIM_SEED"))

Z2M = "zigbee2mqtt"
LAMP, LOCK, MOTION = "livingroom_lamp", "front_door", "hallway_motion"
IVT = "ivt490"
PHONE = "owntracks/alice/phone"


def binary(name, on, off, access=7):
    return {"type": "binary", "name": name, "property": name, "value_on": on,
            "value_off": off, "access": access}


def numeric(name, unit=None, lo=None, hi=None, access=1, category=None):
    expose = {"type": "numeric", "name": name, "property": name, "access": access}
    if unit:
        expose["unit"] = unit
    if lo is not None:
        expose["value_min"], expose["value_max"] = lo, hi
    if category:
        expose["category"] = category
    return expose


INVENTORY = [
    {"ieee_address": "0x00124b0000000000", "type": "Coordinator",
     "friendly_name": "Coordinator", "definition": None},
    {"ieee_address": "0x000d6f0000000001", "type": "Router", "friendly_name": LAMP,
     "definition": {"vendor": "IKEA", "model": "LED1836G9",
                    "description": "TRADFRI bulb E27, dimmable",
                    "exposes": [{"type": "light", "features": [
                        binary("state", "ON", "OFF"),
                        numeric("brightness", lo=0, hi=254, access=7)]},
                        numeric("linkquality", "lqi", 0, 255, category="diagnostic")]}},
    {"ieee_address": "0x000d6f0000000002", "type": "EndDevice", "friendly_name": LOCK,
     "definition": {"vendor": "Danalock", "model": "V3-BTZB",
                    "description": "Smart lock",
                    "exposes": [{"type": "lock", "features": [
                        binary("state", "LOCK", "UNLOCK")]},
                        numeric("battery", "%", 0, 100, category="diagnostic"),
                        numeric("linkquality", "lqi", 0, 255, category="diagnostic")]}},
    {"ieee_address": "0x000d6f0000000003", "type": "EndDevice", "friendly_name": MOTION,
     "definition": {"vendor": "SONOFF", "model": "SNZB-03",
                    "description": "Motion sensor",
                    "exposes": [binary("occupancy", True, False, access=1),
                                numeric("battery", "%", 0, 100, category="diagnostic"),
                                numeric("linkquality", "lqi", 0, 255, category="diagnostic")]}},
]


class House:
    def __init__(self, client: mqtt.Client):
        self.client = client
        self.lamp = {"state": "OFF", "brightness": 180, "linkquality": 120}
        self.lock = {"state": "LOCKED", "battery": 87, "linkquality": 64}
        self.motion = {"occupancy": False, "battery": 93, "linkquality": 88}
        self.setpoint = 21.0
        self.indoor = 20.4
        self.porch = Porch()

    def publish(self, topic, payload, retain=True):
        body = payload if isinstance(payload, str) else json.dumps(payload)
        self.client.publish(topic, body, qos=1, retain=retain)

    def announce(self):
        self.publish(f"{Z2M}/bridge/state", {"state": "online"})
        self.publish(f"{Z2M}/bridge/devices", INVENTORY)
        for name, state in ((LAMP, self.lamp), (LOCK, self.lock), (MOTION, self.motion)):
            self.publish(f"{Z2M}/{name}/availability", {"state": "online"})
            self.publish(f"{Z2M}/{name}", state)
        self.publish(f"{IVT}/controller/state/indoor_temperature_target",
                     {"value": self.setpoint})
        self.publish(f"{IVT}/controller/state/operating_mode", "1")

    # ---- commands, as the devices obey them ----

    def on_command(self, topic: str, payload: bytes):
        try:
            text = payload.decode()
            body = json.loads(text) if text.strip().startswith("{") else text
        except (UnicodeDecodeError, ValueError):
            return
        if topic == f"{Z2M}/{LAMP}/set" and isinstance(body, dict):
            state = body.get("state")
            if state == "TOGGLE":
                state = "OFF" if self.lamp["state"] == "ON" else "ON"
            if state in ("ON", "OFF"):
                self.lamp["state"] = state
            if isinstance(body.get("brightness"), (int, float)):
                self.lamp["brightness"] = max(0, min(254, int(body["brightness"])))
                self.lamp["state"] = "ON" if self.lamp["brightness"] else "OFF"
            self.publish(f"{Z2M}/{LAMP}", self.lamp)
        elif topic == f"{Z2M}/{LOCK}/set" and isinstance(body, dict):
            if body.get("state") in ("LOCK", "UNLOCK"):
                self.lock["state"] = "LOCKED" if body["state"] == "LOCK" else "UNLOCKED"
                self.publish(f"{Z2M}/{LOCK}", self.lock)
        elif topic == f"{IVT}/controller/set/indoor_temperature_target":
            try:
                self.setpoint = round(float(body), 1)
            except (TypeError, ValueError):
                return
            self.publish(f"{IVT}/controller/state/indoor_temperature_target",
                         {"value": self.setpoint})
        elif topic == f"{IVT}/controller/set/operating_mode":
            if str(body).split(".")[0] in ("1", "2", "3"):
                self.publish(f"{IVT}/controller/state/operating_mode", str(body).split(".")[0])

    # ---- the household and the weather ----

    async def hallway(self):
        while True:
            await asyncio.sleep(RNG.uniform(60, 240))
            self.motion["occupancy"] = True
            self.publish(f"{Z2M}/{MOTION}", self.motion)
            await asyncio.sleep(RNG.uniform(45, 120))
            self.motion["occupancy"] = False
            self.publish(f"{Z2M}/{MOTION}", self.motion)

    async def living_room(self):
        while True:
            await asyncio.sleep(RNG.uniform(300, 900))
            if self.lamp["state"] == "OFF":
                self.lamp.update(state="ON", brightness=RNG.choice((90, 140, 200, 254)))
                self.publish(f"{Z2M}/{LAMP}", self.lamp)

    async def front_door(self):
        while True:
            await asyncio.sleep(RNG.uniform(600, 1500))
            self.lock["state"] = "UNLOCKED"
            self.publish(f"{Z2M}/{LOCK}", self.lock)
            await asyncio.sleep(RNG.uniform(40, 90))
            self.lock["state"] = "LOCKED"
            self.publish(f"{Z2M}/{LOCK}", self.lock)

    async def heat_pump(self):
        """Republish every field each cycle, changed or not.

        The real board does the same, about every 32 s.
        """
        last = time.monotonic()
        while True:
            now = time.monotonic()
            dt, last = now - last, now
            hour = time.localtime().tm_hour + time.localtime().tm_min / 60
            outdoor = 5.0 + 4.0 * math.sin(2 * math.pi * (hour - 9) / 24) + RNG.gauss(0, 0.1)
            # A ten-minute time constant toward a little under the setpoint.
            self.indoor += (self.setpoint - 0.2 - self.indoor) * min(1.0, dt / 600)
            self.indoor += RNG.gauss(0, 0.03)
            feed = 22 + 1.1 * max(0.0, self.setpoint - outdoor) + RNG.gauss(0, 0.2)
            self.publish(f"{IVT}/ivt490/state/GT2/filtered", f"{outdoor:.2f}", retain=False)
            self.publish(f"{IVT}/ivt490/state/serial/GT1", f"{feed:.2f}", retain=False)
            self.publish(f"{IVT}/ivt490/state/serial/GT5", f"{feed - 4.5:.2f}", retain=False)
            self.publish(f"{IVT}/controller/state/indoor_temperature_feedback",
                         {"value": round(self.indoor, 2), "valid": True}, retain=False)
            self.publish(f"{IVT}/controller/state/indoor_temperature_target",
                         {"value": self.setpoint})
            self.publish(f"{IVT}/controller/state/operating_mode", "1")
            await asyncio.sleep(32)

    async def alice(self):
        """Home for a while, then a walk around the block, on repeat."""
        battery = 82.0
        while True:
            for _ in range(10):  # five minutes at home
                battery = min(100.0, battery + 0.4)
                self.fix(HOME[0] + RNG.gauss(0, 0.00004), HOME[1] + RNG.gauss(0, 0.00007),
                         acc=12, batt=battery)
                await asyncio.sleep(30)
            steps = 20  # ten minutes walking a ~500 m loop
            for i in range(steps):
                battery = max(5.0, battery - 0.3)
                angle = 2 * math.pi * i / steps
                lat = HOME[0] + 0.0022 * (1 - math.cos(angle))
                lon = HOME[1] + 0.0040 * math.sin(angle)
                self.fix(lat, lon, acc=RNG.choice((6, 8, 15)), batt=battery)
                await asyncio.sleep(30)

    def fix(self, lat, lon, acc, batt):
        self.publish(PHONE, {"_type": "location", "lat": round(lat, 6), "lon": round(lon, 6),
                             "acc": acc, "batt": int(batt), "tst": int(time.time())})


# ---- ESPHome native API: the porch switch ----
# The plaintext framing (a zero byte, varint length, varint message type,
# protobuf body), as in tests/fake_esphome.py. It serves the one entity
# the starter house binds: switch `relay` on device `porch`.

HELLO_REQUEST, HELLO_RESPONSE = 1, 2
DISCONNECT_REQUEST, DISCONNECT_RESPONSE = 5, 6
PING_REQUEST, PING_RESPONSE = 7, 8
DEVICE_INFO_REQUEST, DEVICE_INFO_RESPONSE = 9, 10
LIST_ENTITIES_REQUEST = 11
LIST_ENTITIES_SWITCH_RESPONSE = 17
LIST_ENTITIES_DONE_RESPONSE = 19
SUBSCRIBE_STATES_REQUEST = 20
SWITCH_STATE_RESPONSE = 26
SWITCH_COMMAND_REQUEST = 33
RELAY_KEY = 1


def varint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte, value = value & 0x7F, value >> 7
        out.append(byte | 0x80 if value else byte)
        if not value:
            return bytes(out)


async def read_varint(reader) -> int:
    result = shift = 0
    while True:
        byte = (await reader.readexactly(1))[0]
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result
        shift += 7


def frame(msg_type: int, message) -> bytes:
    data = message.SerializeToString()
    return b"\x00" + varint(len(data)) + varint(msg_type) + data


class Porch:
    def __init__(self):
        self.on = False
        self.subscribers = set()

    def state(self) -> bytes:
        return frame(SWITCH_STATE_RESPONSE, api_pb2.SwitchStateResponse(key=RELAY_KEY, state=self.on))

    async def serve(self, reader, writer):
        try:
            while True:
                try:
                    if await read_varint(reader) != 0:
                        return
                    length = await read_varint(reader)
                    msg_type = await read_varint(reader)
                    data = await reader.readexactly(length) if length else b""
                except (asyncio.IncompleteReadError, ConnectionError):
                    return
                if msg_type == HELLO_REQUEST:
                    writer.write(frame(HELLO_RESPONSE, api_pb2.HelloResponse(
                        api_version_major=1, api_version_minor=9,
                        server_info="homeostat demo", name="porch")))
                elif msg_type == DEVICE_INFO_REQUEST:
                    writer.write(frame(DEVICE_INFO_RESPONSE, api_pb2.DeviceInfoResponse(
                        name="porch", friendly_name="Porch", mac_address="02:00:00:00:00:01",
                        model="esp32 (simulated)", esphome_version="2025.1.0")))
                elif msg_type == LIST_ENTITIES_REQUEST:
                    writer.write(frame(LIST_ENTITIES_SWITCH_RESPONSE, api_pb2.ListEntitiesSwitchResponse(
                        object_id="relay", key=RELAY_KEY, name="Porch light")))
                    writer.write(frame(LIST_ENTITIES_DONE_RESPONSE, api_pb2.ListEntitiesDoneResponse()))
                elif msg_type == SUBSCRIBE_STATES_REQUEST:
                    self.subscribers.add(writer)
                    writer.write(self.state())
                elif msg_type == SWITCH_COMMAND_REQUEST:
                    command = api_pb2.SwitchCommandRequest()
                    command.ParseFromString(data)
                    self.on = command.state
                    for subscriber in set(self.subscribers):
                        subscriber.write(self.state())
                elif msg_type == PING_REQUEST:
                    writer.write(frame(PING_RESPONSE, api_pb2.PingResponse()))
                elif msg_type == DISCONNECT_REQUEST:
                    writer.write(frame(DISCONNECT_RESPONSE, api_pb2.DisconnectResponse()))
                    await writer.drain()
                    return
                await writer.drain()
        finally:
            self.subscribers.discard(writer)
            writer.close()


async def main():
    loop = asyncio.get_running_loop()
    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="homeostat-demo-devices")
    house = House(client)

    def on_connect(client, userdata, flags, reason_code, properties=None):
        if reason_code.is_failure:
            print(f"broker refused the connection: {reason_code}", flush=True)
            return
        client.subscribe([(f"{Z2M}/+/set", 1), (f"{IVT}/controller/set/+", 1)])
        loop.call_soon_threadsafe(house.announce)
        print(f"connected to {MQTT_HOST}:{MQTT_PORT}; devices announced", flush=True)

    def on_message(client, userdata, msg):
        loop.call_soon_threadsafe(house.on_command, msg.topic, msg.payload)

    client.on_connect = on_connect
    client.on_message = on_message
    client.will_set(f"{Z2M}/bridge/state", json.dumps({"state": "offline"}), retain=True)
    # The broker may still be starting: retry until it answers.
    while True:
        try:
            client.connect(MQTT_HOST, MQTT_PORT)
            break
        except OSError as err:
            print(f"waiting for the broker at {MQTT_HOST}:{MQTT_PORT}: {err}", flush=True)
            await asyncio.sleep(2)
    client.loop_start()

    server = await asyncio.start_server(house.porch.serve, "0.0.0.0", ESPHOME_PORT)
    print(f"porch switch (ESPHome) listening on :{ESPHOME_PORT}", flush=True)
    async with server:
        await asyncio.gather(
            server.serve_forever(),
            house.hallway(),
            house.living_room(),
            house.front_door(),
            house.heat_pump(),
            house.alice(),
        )


if __name__ == "__main__":
    asyncio.run(main())
