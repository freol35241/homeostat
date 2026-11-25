"""SilenceWatch: pinned against an injected clock and a fake session, no bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import threading
import unittest

from homeostat.availability import SilenceWatch
from homeostat.house import Entity
from homeostat.testing import FakeSession

STOVE = Entity(name="stove", id="aduro/stove1", capability="burner", room="living")
PUMP = Entity(name="pump", id="ivt490/pump1", capability="climate", room="cellar")
STOVE_AVAILABLE = "home/state/living/stove/available"
PUMP_AVAILABLE = "home/state/cellar/pump/available"
STOVE_SILENT = {"kind": "device-silent", "topic": STOVE.id}


class ScriptedStop:
    """Stands in for the watch's stop event and records each wait.

    `wait` returns False (time to sweep) `rounds` times, then True.
    """

    def __init__(self, rounds):
        self.rounds = rounds
        self.waits = []

    def wait(self, timeout):
        self.waits.append(timeout)
        return len(self.waits) > self.rounds

    def set(self):
        pass


class SilenceWatchTest(unittest.TestCase):
    def setUp(self):
        self.now = 0.0
        self.session = FakeSession()
        self.watch = SilenceWatch(self.session, [STOVE, PUMP], clock=lambda: self.now)

    def silent_events(self):
        return [e for e in self.session.events if e["kind"] == "device-silent"]

    def test_first_contact_is_available_once_and_reported_first(self):
        self.assertFalse(self.watch.heard(STOVE))
        self.assertTrue(self.watch.contact(STOVE))
        self.assertFalse(self.watch.contact(STOVE))
        self.assertTrue(self.watch.heard(STOVE))
        self.assertFalse(self.watch.heard(PUMP))
        self.assertEqual(self.session.puts, [(STOVE_AVAILABLE, True)])

    def test_silence_is_one_unavailable_and_one_event(self):
        self.watch.contact(STOVE)
        self.now = 200.0
        self.watch.contact(PUMP)
        self.now = 301.0
        self.watch.sweep(300.0)
        self.watch.sweep(300.0)
        self.assertEqual(self.session.puts[-1], (STOVE_AVAILABLE, False))
        self.assertEqual(self.silent_events(), [STOVE_SILENT])

    def test_an_entity_never_heard_goes_unavailable_after_one_timeout(self):
        self.now = 300.0
        self.watch.sweep(300.0)
        self.assertEqual(self.session.puts, [])
        self.now = 301.0
        self.watch.sweep(300.0)
        self.assertEqual(self.session.puts, [(STOVE_AVAILABLE, False), (PUMP_AVAILABLE, False)])

    def test_the_next_message_makes_it_available_again(self):
        self.watch.contact(STOVE)
        self.now = 301.0
        self.watch.sweep(300.0)
        self.assertFalse(self.watch.contact(STOVE))
        self.assertEqual(self.session.puts[-1], (STOVE_AVAILABLE, True))
        self.now = 602.0
        self.watch.sweep(300.0)
        self.assertEqual(self.silent_events().count(STOVE_SILENT), 2)

    def test_a_message_during_a_sweep_leaves_the_entity_available(self):
        # A message arrives on another thread just as the sweep reads the
        # stove's last contact. Its contact waits for the sweep's lock, so
        # it lands after the put and marks the stove available again.
        self.watch.contact(STOVE)
        self.now = 301.0
        # Set when the message's contact has finished or is waiting for
        # the lock, so the sweep goes on without a timed wait.
        progressed = threading.Event()

        class SignallingLock:
            def __init__(self):
                self._lock = threading.Lock()

            def __enter__(self):
                if not self._lock.acquire(blocking=False):
                    progressed.set()
                    self._lock.acquire()

            def __exit__(self, *exc):
                self._lock.release()

        def message():
            self.watch.contact(STOVE)
            progressed.set()

        receiver = threading.Thread(target=message, daemon=True)

        class LastContact(dict):
            def __getitem__(self, key):
                value = super().__getitem__(key)
                if key == STOVE.id and not progressed.is_set():
                    receiver.start()
                    progressed.wait(5)
                return value

        self.watch._lock = SignallingLock()  # type: ignore[assignment]
        self.watch._last_rx = LastContact(self.watch._last_rx)
        self.watch.sweep(300.0)
        receiver.join(5)
        self.assertFalse(receiver.is_alive())
        stove = [value for key, value in self.session.puts if key == STOVE_AVAILABLE]
        self.assertEqual(stove, [True, False, True])

    def test_the_watchdog_reads_the_timeout_each_round_and_waits_a_quarter_of_it(self):
        # Silent for 1 s throughout. Only the third timeout is short enough
        # to sweep the stove unavailable.
        watch = SilenceWatch(self.session, [STOVE], clock=lambda: self.now)
        self.now = 1.0
        timeouts = iter([8.0, 2.0, 0.4, 0.4])
        reads = []

        def timeout():
            reads.append(next(timeouts))
            return reads[-1]

        stop = ScriptedStop(rounds=3)
        watch._stop = stop  # type: ignore[assignment]
        watch.start(timeout)
        watch.stop()
        self.assertEqual(reads, [8.0, 2.0, 0.4, 0.4])
        self.assertEqual(stop.waits, [1.0, 0.5, 0.1, 0.1])
        self.assertEqual(self.silent_events(), [STOVE_SILENT])

    def test_the_watchdog_thread_sweeps_and_stop_ends_it(self):
        fired = threading.Event()
        record = self.session.health_event

        def health_event(kind, **fields):
            record(kind, **fields)
            fired.set()

        self.session.health_event = health_event
        watch = SilenceWatch(self.session, [STOVE], clock=lambda: self.now)
        self.now = 1.0
        watch.start(lambda: 0.1)
        self.assertTrue(fired.wait(5))
        watch.stop()
        self.assertEqual(self.silent_events(), [STOVE_SILENT])
        thread = watch._thread
        assert thread is not None
        self.assertFalse(thread.is_alive())


if __name__ == "__main__":
    unittest.main()
