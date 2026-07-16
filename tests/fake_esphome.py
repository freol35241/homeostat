# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "aioesphomeapi>=45,<46",
# ]
# ///
"""A minimal, honest ESPHome native-API device, for the esphome adapter's
integration tests (tests/esphome.rs; see docs/design.md, "ESPHome adapter
(settled 2026-07-16)").

Speaks the real plaintext wire protocol (a zero byte, a varint length, a
varint message-type id, then the protobuf payload — docs/design.md cites no
framing detail, but aioesphomeapi's own frame helper does; this mirrors it
by hand rather than reaching into aioesphomeapi's internals) using the
protobuf message classes bundled in aioesphomeapi.api_pb2, so the bytes on
the wire are exactly what a real device would send. Encryption (Noise) is
deliberately not implemented here — the devices-file key plumbing is
exercised without a live encrypted peer (see the adapter's module
docstring).

Fixed inventory, just enough for the adapter's v1 vocabulary:
  - switch "relay" (key 1)
  - sensor "temperature" (key 2), device_class "temperature"
  - binary_sensor "motion" (key 3), device_class "motion"

Handles: HelloRequest, DeviceInfoRequest, ListEntitiesRequest (+ Done),
SubscribeStatesRequest (sends the current state of every entity, then any
future change), SwitchCommandRequest (updates state and echoes it back to
every subscribed connection, like a real relay), PingRequest. Anything else
is ignored, never crashes the server.
"""

import argparse
import asyncio
import os

from aioesphomeapi import api_pb2

# Message-type ids are a stable, protocol-level fact (docs/design.md leaves
# framing to the library; these numbers are aioesphomeapi's own
# core.MESSAGE_TYPE_TO_PROTO for the subset this fake speaks).
HELLO_REQUEST, HELLO_RESPONSE = 1, 2
DISCONNECT_REQUEST, DISCONNECT_RESPONSE = 5, 6
PING_REQUEST, PING_RESPONSE = 7, 8
DEVICE_INFO_REQUEST, DEVICE_INFO_RESPONSE = 9, 10
LIST_ENTITIES_REQUEST = 11
LIST_ENTITIES_BINARY_SENSOR_RESPONSE = 12
LIST_ENTITIES_SENSOR_RESPONSE = 16
LIST_ENTITIES_SWITCH_RESPONSE = 17
LIST_ENTITIES_DONE_RESPONSE = 19
SUBSCRIBE_STATES_REQUEST = 20
BINARY_SENSOR_STATE_RESPONSE = 21
SENSOR_STATE_RESPONSE = 25
SWITCH_STATE_RESPONSE = 26
SWITCH_COMMAND_REQUEST = 33

SWITCH_KEY, SENSOR_KEY, BINARY_SENSOR_KEY = 1, 2, 3


def encode_varint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


async def read_varint(reader: asyncio.StreamReader) -> int:
    result = 0
    shift = 0
    while True:
        byte = (await reader.readexactly(1))[0]
        result |= (byte & 0x7F) << shift
        if not (byte & 0x80):
            return result
        shift += 7


def encode_message(msg_type: int, message) -> bytes:
    data = message.SerializeToString()
    return b"\x00" + encode_varint(len(data)) + encode_varint(msg_type) + data


async def read_message(reader: asyncio.StreamReader) -> tuple[int, bytes]:
    preamble = await read_varint(reader)
    if preamble != 0:
        raise ValueError(f"bad preamble {preamble}")
    length = await read_varint(reader)
    msg_type = await read_varint(reader)
    data = await reader.readexactly(length) if length else b""
    return msg_type, data


class Device:
    """Shared state across connections (a real device has exactly one, but
    the test harness is honest about it being possible to reconnect)."""

    def __init__(self, name: str):
        self.name = name
        self.state = {"relay": False, "temperature": 21.5, "motion": True}
        self.subscribers: set[asyncio.StreamWriter] = set()

    def broadcast_switch(self) -> None:
        msg = api_pb2.SwitchStateResponse(key=SWITCH_KEY, state=self.state["relay"])
        for writer in set(self.subscribers):
            writer.write(encode_message(SWITCH_STATE_RESPONSE, msg))


async def send_entity_list(writer: asyncio.StreamWriter) -> None:
    writer.write(
        encode_message(
            LIST_ENTITIES_SWITCH_RESPONSE,
            api_pb2.ListEntitiesSwitchResponse(object_id="relay", key=SWITCH_KEY, name="Relay"),
        )
    )
    writer.write(
        encode_message(
            LIST_ENTITIES_SENSOR_RESPONSE,
            api_pb2.ListEntitiesSensorResponse(
                object_id="temperature",
                key=SENSOR_KEY,
                name="Temperature",
                device_class="temperature",
                unit_of_measurement="°C",
                accuracy_decimals=1,
            ),
        )
    )
    writer.write(
        encode_message(
            LIST_ENTITIES_BINARY_SENSOR_RESPONSE,
            api_pb2.ListEntitiesBinarySensorResponse(
                object_id="motion",
                key=BINARY_SENSOR_KEY,
                name="Motion",
                device_class="motion",
            ),
        )
    )
    writer.write(encode_message(LIST_ENTITIES_DONE_RESPONSE, api_pb2.ListEntitiesDoneResponse()))
    await writer.drain()


async def send_states(writer: asyncio.StreamWriter, device: Device) -> None:
    writer.write(
        encode_message(
            SWITCH_STATE_RESPONSE,
            api_pb2.SwitchStateResponse(key=SWITCH_KEY, state=device.state["relay"]),
        )
    )
    writer.write(
        encode_message(
            SENSOR_STATE_RESPONSE,
            api_pb2.SensorStateResponse(key=SENSOR_KEY, state=device.state["temperature"]),
        )
    )
    writer.write(
        encode_message(
            BINARY_SENSOR_STATE_RESPONSE,
            api_pb2.BinarySensorStateResponse(key=BINARY_SENSOR_KEY, state=device.state["motion"]),
        )
    )
    await writer.drain()


def make_handler(device: Device):
    async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            while True:
                try:
                    msg_type, data = await read_message(reader)
                except (asyncio.IncompleteReadError, ConnectionResetError, ValueError):
                    return

                if msg_type == HELLO_REQUEST:
                    resp = api_pb2.HelloResponse(
                        api_version_major=1, api_version_minor=9,
                        server_info="fake-esphome", name=device.name,
                    )
                    writer.write(encode_message(HELLO_RESPONSE, resp))
                    await writer.drain()
                elif msg_type == DEVICE_INFO_REQUEST:
                    resp = api_pb2.DeviceInfoResponse(
                        name=device.name, friendly_name=device.name,
                        mac_address="AA:BB:CC:DD:EE:FF", model="fake",
                        esphome_version="2024.1.0", uses_password=False,
                    )
                    writer.write(encode_message(DEVICE_INFO_RESPONSE, resp))
                    await writer.drain()
                elif msg_type == LIST_ENTITIES_REQUEST:
                    await send_entity_list(writer)
                elif msg_type == SUBSCRIBE_STATES_REQUEST:
                    device.subscribers.add(writer)
                    await send_states(writer, device)
                elif msg_type == SWITCH_COMMAND_REQUEST:
                    cmd = api_pb2.SwitchCommandRequest()
                    cmd.ParseFromString(data)
                    device.state["relay"] = cmd.state
                    device.broadcast_switch()
                    await writer.drain()
                elif msg_type == PING_REQUEST:
                    writer.write(encode_message(PING_RESPONSE, api_pb2.PingResponse()))
                    await writer.drain()
                elif msg_type == DISCONNECT_REQUEST:
                    writer.write(encode_message(DISCONNECT_RESPONSE, api_pb2.DisconnectResponse()))
                    await writer.drain()
                    return
                # else: unhandled message type — real devices ignore what
                # they don't support too; never crash on it.
        finally:
            device.subscribers.discard(writer)
            writer.close()

    return handle


async def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=int(os.environ.get("FAKE_ESPHOME_PORT", "6053")))
    parser.add_argument("--name", default="fake-esphome")
    args = parser.parse_args()

    device = Device(args.name)
    server = await asyncio.start_server(make_handler(device), "127.0.0.1", args.port)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
