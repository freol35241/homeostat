"""The OpenWrt translation, without a router: polls in, WAN and sightings out."""

import asyncio
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from homeostat.testing import FakeSample, FakeSession

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

import openwrt
from openwrt import Adapter, UbusError, classify, router_place

PHONE = "aa:bb:cc:dd:ee:ff"
STATE = "home/state/person/alice_wifi"


def entity(name, capability, dev_id, room="network"):
    return SimpleNamespace(name=name, room=room, capability=capability, id=dev_id)


class RouterPlaceTest(unittest.TestCase):
    def test_a_router_without_a_location_has_no_place(self):
        self.assertIsNone(router_place("gw", {"host": "10.0.0.1"}))

    def test_a_location_takes_its_radius_or_the_default(self):
        self.assertEqual(router_place("gw", {"lat": 59, "lon": 18}), (59.0, 18.0, 50.0))
        self.assertEqual(router_place("gw", {"lat": 59, "lon": 18, "radius_m": 20}), (59.0, 18.0, 20.0))

    def test_a_malformed_location_is_an_error(self):
        for conf in (
            {"radius_m": 20},
            {"lat": 59},
            {"lon": 18},
            {"lat": 91, "lon": 18},
            {"lat": 59, "lon": -181},
            {"lat": True, "lon": 18},
            {"lat": "59", "lon": 18},
            {"lat": float("nan"), "lon": 18},
            {"lat": 59, "lon": 18, "radius_m": 0},
            {"lat": 59, "lon": 18, "radius_m": float("inf")},
            {"lat": 59, "lon": 18, "radius_m": True},
        ):
            with self.subTest(conf=conf), self.assertRaises(ValueError):
                router_place("gw", conf)


class ClassifyTest(unittest.TestCase):
    def test_usable_bindings_are_split_by_capability(self):
        session = FakeSession("openwrt")
        gw = entity("gateway", "router", "gw")
        phone = entity("alice_wifi", "tracker", PHONE, room="person")
        self.assertEqual(classify([gw, phone], {"gw": {}}, session), ([gw], [phone]))
        self.assertEqual(session.events, [])

    def test_an_unusable_binding_drops_with_its_reason(self):
        for bound, reason in (
            (entity("gateway", "router", "ap"), "router-unconfigured"),
            (entity("alice_wifi", "tracker", PHONE.upper()), "malformed-id"),
            (entity("lamp", "light", "x"), "unsupported-capability"),
        ):
            with self.subTest(reason=reason):
                session = FakeSession("openwrt")
                self.assertEqual(classify([bound], {"gw": {}}, session), ([], []))
                self.assertEqual(
                    [(e["kind"], e["reason"]) for e in session.events], [("drop", reason)]
                )


class CycleTest(unittest.TestCase):
    """`Adapter.cycle` over two routers whose polls the test answers."""

    def setUp(self):
        self.session = FakeSession("openwrt")
        self.routers = {
            "gw": {"host": "10.0.0.1", "lat": 59.0, "lon": 18.0, "radius_m": 30},
            "ap": {"host": "10.0.0.2", "lat": 59.1, "lon": 18.1},
        }
        self.answers: dict = {}
        self.clock = SimpleNamespace(time=lambda: 1000.0 + self.mono, monotonic=lambda: self.mono)
        self.mono = 0.0
        patcher = mock.patch.multiple(openwrt, poll_router=self.poll, time=self.clock)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.adapter = Adapter(
            self.session,
            self.routers,
            [entity("gateway", "router", "gw")],
            [entity("alice_wifi", "tracker", PHONE, room="person")],
        )

    async def poll(self, http, name, conf):
        answer = self.answers[name]
        if isinstance(answer, Exception):
            raise answer
        return answer

    def answer(self, gw, ap, wan=True):
        """Set each router's answer: a set of stations, or an exception."""
        for name, stations in (("gw", gw), ("ap", ap)):
            if isinstance(stations, Exception):
                self.answers[name] = stations
            else:
                interfaces = {"wan": {"up": wan}} if wan is not None else {"lan": {"up": True}}
                self.answers[name] = (interfaces, set(stations))

    def away_delay(self, seconds):
        self.session.deliver(FakeSample("home/config/openwrt/away_delay_s", seconds))

    def cycle(self, at=None):
        if at is not None:
            self.mono = at
        before = len(self.session.puts)
        asyncio.run(self.adapter.cycle(None))
        return [
            (key.removeprefix("home/state/"), value)
            for key, value in self.session.puts[before:]
            if key.startswith("home/state/")
        ]

    def kinds(self):
        return [e["kind"] for e in self.session.events]

    def test_the_first_cycle_publishes_wan_a_fix_and_the_sighting(self):
        self.answer(gw=[PHONE], ap=[PHONE])
        self.assertEqual(
            self.cycle(),
            [
                ("network/gateway/wan", True),
                ("person/alice_wifi/lat", 59.0),
                ("person/alice_wifi/lon", 18.0),
                ("person/alice_wifi/accuracy", 30.0),
                ("person/alice_wifi/fixed_at", 1000.0),
                ("person/alice_wifi/sighted", True),
                ("person/alice_wifi/available", True),
            ],
        )

    def test_only_changes_publish(self):
        self.answer(gw=[PHONE], ap=[])
        self.cycle()
        puts = len(self.session.puts)
        self.assertEqual(self.cycle(at=10), [])
        self.assertEqual(len(self.session.puts), puts)
        self.answer(gw=[PHONE], ap=[], wan=False)
        self.assertEqual(self.cycle(at=20), [("network/gateway/wan", False)])

    def test_a_fix_is_placed_on_coming_into_range_only(self):
        self.away_delay(0)
        self.answer(gw=[], ap=[PHONE])
        self.assertIn(("person/alice_wifi/accuracy", 50.0), self.cycle())
        self.assertEqual(self.cycle(at=10), [])
        self.answer(gw=[], ap=[])
        self.assertEqual(self.cycle(at=20), [("person/alice_wifi/sighted", False)])
        self.answer(gw=[], ap=[PHONE])
        self.assertEqual(
            self.cycle(at=30),
            [("person/alice_wifi/fixed_at", 1030.0), ("person/alice_wifi/sighted", True)],
        )

    def test_a_sighting_is_held_for_away_delay(self):
        self.answer(gw=[PHONE], ap=[])
        self.cycle()
        self.answer(gw=[], ap=[])
        self.assertEqual(self.cycle(at=179), [])
        self.assertEqual(self.cycle(at=180), [("person/alice_wifi/sighted", False)])

    def test_an_unreachable_router_reports_once_per_down_transition(self):
        self.answer(gw=UbusError("timeout"), ap=[])
        self.cycle()
        self.cycle(at=10)
        self.answer(gw=[], ap=[])
        self.cycle(at=20)
        self.answer(gw=ValueError("clients is a list"), ap=[])
        self.cycle(at=30)
        self.assertEqual(
            [k for k in self.kinds() if k.startswith("router-")],
            ["router-unreachable", "router-poll-failed"],
        )

    def test_with_every_router_silent_only_availability_publishes(self):
        self.answer(gw=[PHONE], ap=[])
        self.cycle()
        self.answer(gw=UbusError("timeout"), ap=UbusError("timeout"))
        self.assertEqual(self.cycle(at=10), [("person/alice_wifi/available", False)])

    def test_a_silent_router_makes_an_unsighted_tracker_unavailable(self):
        self.answer(gw=[PHONE], ap=[])
        self.cycle()
        self.away_delay(0)
        self.answer(gw=[], ap=UbusError("timeout"))
        self.assertEqual(self.cycle(at=10), [("person/alice_wifi/available", False)])
        self.cycle(at=20)
        partial = [e for e in self.session.events if e["kind"] == "presence-partial"]
        self.assertEqual(partial, [{"kind": "presence-partial", "routers": ["ap"]}])

    def test_a_sighting_counts_while_another_router_is_silent(self):
        self.answer(gw=[PHONE], ap=UbusError("timeout"))
        published = dict(self.cycle())
        self.assertEqual(
            (published["person/alice_wifi/sighted"], published["person/alice_wifi/available"]),
            (True, True),
        )

    def test_a_dump_without_wan_reports_unknown_interface_once_per_loss(self):
        self.answer(gw=[], ap=[], wan=None)
        self.assertNotIn(("network/gateway/wan", True), self.cycle())
        self.cycle(at=10)
        self.answer(gw=[], ap=[])
        self.assertIn(("network/gateway/wan", True), self.cycle(at=20))
        self.answer(gw=[], ap=[], wan=None)
        self.cycle(at=30)
        self.assertEqual(self.kinds().count("unknown-interface"), 2)


if __name__ == "__main__":
    unittest.main()
