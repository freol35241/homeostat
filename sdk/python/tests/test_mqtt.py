"""The MQTT message guard: what it drops, and what it says when it does.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import unittest

import paho.mqtt.client as mqtt
from homeostat.mqtt import guard


def message(topic: bytes) -> mqtt.MQTTMessage:
    """A paho message carrying raw topic bytes, the way the network loop
    hands one to the callback: `topic` is a property that decodes `_topic`
    lazily, so an undecodable one raises at access, not here."""
    msg = mqtt.MQTTMessage(topic=topic)
    msg.payload = b"{}"
    return msg


class GuardTest(unittest.TestCase):
    def setUp(self):
        self.events = []
        self.seen = []

    def health(self, kind, **fields):
        self.events.append((kind, fields))

    def on_message(self, client, userdata, msg):
        self.seen.append(msg.topic)

    def test_a_good_topic_reaches_the_adapter(self):
        guard(self.on_message, self.health)(None, None, message(b"ivt490/state/x"))
        self.assertEqual(self.seen, ["ivt490/state/x"])
        self.assertEqual(self.events, [])

    def test_an_undecodable_topic_is_a_typed_drop_carrying_the_bytes(self):
        # The byte observed at VP52: 0x82 is not a valid UTF-8 start byte.
        guard(self.on_message, self.health)(None, None, message(b"ivt490/\x82state"))
        self.assertEqual(self.seen, [])  # never reaches the adapter
        self.assertEqual(len(self.events), 1)
        kind, fields = self.events[0]
        self.assertEqual(kind, "drop")
        self.assertEqual(fields["reason"], "malformed-topic")
        self.assertIn(r"\x82", fields["topic"])  # the bytes, for counting

    def test_a_long_topic_is_truncated_before_it_reaches_the_bus(self):
        guard(self.on_message, self.health)(None, None, message(b"\x82" + b"a" * 500))
        self.assertLessEqual(len(self.events[0][1]["topic"]), 120)

    def test_an_adapter_raising_does_not_kill_the_callback(self):
        def boom(client, userdata, msg):
            raise ValueError("adapter bug")

        guard(boom, self.health)(None, None, message(b"ivt490/state/x"))
        # Survived, and stays a trace: an adapter bug is not a bus drop.
        self.assertEqual(self.events, [])

    def test_without_a_health_hook_the_drop_still_does_not_raise(self):
        guard(self.on_message)(None, None, message(b"ivt490/\x82state"))
        self.assertEqual(self.seen, [])


if __name__ == "__main__":
    unittest.main()
