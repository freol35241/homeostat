"""ntfy's admission rule, without a server: which wishes become sends."""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from ntfy import Gate, Send

MESSAGE = "home/cmd/person/alice_phone/message"
ALERT = "home/cmd/person/alice_phone/alert"


def wish(value, actor="scheduler", cmd_id="c0ffee01"):
    return json.dumps(
        {"value": value, "priority": "automation", "actor": actor, "id": cmd_id}
    ).encode()


class GateTest(unittest.TestCase):
    def setUp(self):
        self.now = 100.0
        self.gate = Gate("ntfy", lambda: 5.0, clock=lambda: self.now)

    def admit(self, key, raw):
        return self.gate.admit("alice_phone", key, raw)

    def test_a_message_is_sent_titled_by_its_actor(self):
        self.assertEqual(
            self.admit(MESSAGE, wish("dinner")), Send("message", "dinner", "scheduler", "c0ffee01")
        )

    def test_an_unprintable_actor_falls_back_to_the_unit(self):
        self.assertEqual(self.admit(MESSAGE, wish("hi", actor="evil\r\nX: y")).title, "ntfy")
        self.assertEqual(self.admit(ALERT, wish("hi", actor="å")).title, "ntfy")

    def test_what_is_not_a_send_drops_with_its_reason(self):
        cases = [
            (MESSAGE, b"{", "malformed-payload"),
            (MESSAGE, json.dumps("bare").encode(), "invalid-command"),
            (MESSAGE, wish(42), "invalid-command"),
            (MESSAGE, wish("   "), "invalid-command"),
            ("home/cmd/person/alice_phone/ring", wish("hi"), "invalid-command"),
        ]
        for key, raw, reason in cases:
            with self.subTest(raw=raw, key=key):
                drop = self.admit(key, raw)
                self.assertEqual(drop["reason"], reason)
                self.assertEqual(drop["key"], key)

    def test_a_drop_names_its_command(self):
        self.assertEqual(self.admit(MESSAGE, wish(42))["cmd_id"], "c0ffee01")

    def test_messages_inside_the_floor_are_rate_limited(self):
        self.assertIsInstance(self.admit(MESSAGE, wish("first")), Send)
        self.now += 4.9
        drop = self.admit(MESSAGE, wish("second"))
        self.assertEqual((drop["reason"], drop["min_interval_s"]), ("rate-limited", 5.0))
        self.now += 0.2
        self.assertIsInstance(self.admit(MESSAGE, wish("third")), Send)

    def test_alerts_are_never_rate_limited(self):
        for _ in range(3):
            self.assertIsInstance(self.admit(ALERT, wish("smoke")), Send)

    def test_the_floor_is_per_entity(self):
        self.assertIsInstance(self.gate.admit("alice_phone", MESSAGE, wish("a")), Send)
        bob = "home/cmd/person/bob_phone/message"
        self.assertIsInstance(self.gate.admit("bob_phone", bob, wish("b")), Send)


if __name__ == "__main__":
    unittest.main()
