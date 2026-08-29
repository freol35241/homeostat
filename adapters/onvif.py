# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "aiohttp>=3.9,<4",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""ONVIF camera adapter (see docs/design.md, "Cameras (settled
2026-07-19)").

Named for the dialect it speaks, not the vendor: Profile S pull-point
events only — no PTZ, no imaging service, no capability negotiation. The
event plane is the whole job: on-camera detections normalize to scalar bus
aspects (v1: `motion`, a bool) at home/state/{room}/{entity}/motion.
Pixels never pass through here — the media plane is go2rtc's (see
adapters/go2rtc.py).

The entity file's `id` is the camera's key into HOMEOSTAT_CAMERAS, a TOML
file outside the repo carrying per-camera `host` (optionally `host:port`;
the port default is Tapo's ONVIF 2020), `username`, and `password` — the
camera-account credentials created in the vendor app. Addresses and
passwords never enter the repo. A camera with no entry drops with a health
event and is skipped; the other cameras are unaffected.

The SOAP layer is hand-rolled (the MCP precedent: an ONVIF/WS-* client
library would be the largest dependency in the tree for four calls):
CreatePullPointSubscription, PullMessages (a long poll), Renew, and a
WS-Security UsernameToken digest header on each. Tapo firmware has broken
pull-point subscriptions before (the 1.3.6 regression), so ANY fault on
the event stream — HTTP error, SOAP fault, timeout, unparseable envelope —
tears the subscription down and recreates it from scratch, with one
"event-stream-lost" health event per down transition, never a crash. Every
error names the call that produced it and carries the fault's reason: a
camera that accepts CreatePullPointSubscription and rejects Renew is a
different problem from one that rejects the subscribe, and a bare status
code cannot tell them apart from outside the process. The retry backs off
from RESUBSCRIBE_DELAY_S toward RESUBSCRIBE_MAX_S and the event carries
the consecutive-failure count, because a camera whose subscribe succeeds
and whose stream then fails oscillates -- up flips back on each new
subscription, so "one per down transition" would otherwise mean one per
cycle, forever, against a live camera. A COMPLETED round trip resets
both -- a successful subscribe alone does not, or a camera that refuses
only Renew would reset the count every cycle and never back off. A notification that parses but carries an unusable value
drops with a "malformed-payload" health event and the stream continues.

`motion` is published on CHANGE, not per notification. A notification is
not a transition: a Tapo C200 sends MotionAlarm on every evaluation tick,
so one real episode against VP52's cameras arrived as 417 identical `true`s
in 56 seconds — 456 recorded rows for what is semantically two edges. The
adapter therefore compares against the last value it published and stays
silent otherwise, which is also what makes it behave like the other
event-driven adapters, where the device itself speaks only on change.

The same transitions carry the availability signal (docs/design.md,
"Sensor dropout and availability"): a working pull-point subscription
publishes home/state/{room}/{entity}/available = true, its loss publishes
false — and `motion` stands untouched on loss, stale, never false.

The camera may return a subscription address with an unroutable host (NAT,
container namespaces); only its path and query are trusted — the netloc
stays the configured one. This is load-bearing, not defensive: a Tapo
tested in the field advertises a per-subscription port (1024, 1025, ...)
that nothing can connect to, so without the rewrite every call after the
subscribe would time out.

Renew and Unsubscribe are the WS-BaseNotification SubscriptionManager
operations, and firmware that serves pull points happily may implement
NEITHER — the same Tapo answers CreatePullPointSubscription and
PullMessages with 200 and both of those with 400, and its own
GetServiceCapabilities reports the SubscriptionManager interfaces absent.
A Renew fault has two causes and they must be told apart: firmware with no
SubscriptionManager, where the stream is fine, or a subscription that is
genuinely gone, where it is dead. The NEXT pull decides — it succeeds in
the first case and fails in the second — so the adapter withholds judgment
for one round trip rather than concluding from the fault alone. On the
first reading it stops renewing that camera, keeps pulling, and rotates the
subscription RESUBSCRIBE_BEFORE_S before InitialTerminationTime expires,
unsubscribing the old one best-effort; availability does not flap, because
nothing was lost. On the second the ordinary loss path runs. Learned from
behaviour rather than negotiated: no capability calls, per the scope above.
"""

import asyncio
import base64
import contextlib
import hashlib
import os
import signal
import tomllib
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit
from xml.sax.saxutils import escape
from xml.etree import ElementTree

import aiohttp

import homeostat
from homeostat import house, keys

ENV_CAMERAS = "HOMEOSTAT_CAMERAS"
DEFAULT_PORT = 2020  # Tapo's ONVIF service port; override per camera with host:port
PULL_TIMEOUT = "PT10S"
TERMINATION_TIME = "PT60S"
TERMINATION_S = 60
# Re-create a subscription this long before it expires, for cameras with
# no working Renew. The old one lingers until it times out, so this also
# bounds the overlap: at 40 s against a PT60S termination, at most two per
# camera are live at once, well under the MaxPullPoints these firmwares
# advertise.
RESUBSCRIBE_BEFORE_S = 40
RESUBSCRIBE_DELAY_S = 5
# A camera that rejects one call rejects it again: back off toward
# RESUBSCRIBE_MAX_S rather than hammering a live camera at a fixed cadence
# forever. Any success resets it.
RESUBSCRIBE_MAX_S = 300
HTTP_TIMEOUT_S = 30  # must exceed the PT10S long poll
# A SOAP fault's reason lives in the body; enough of it to be diagnostic,
# bounded because it is going into a health event.
FAULT_EXCERPT = 300

SOAP_ENV = "http://www.w3.org/2003/05/soap-envelope"
WSSE = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"
WSU = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"
PASSWORD_DIGEST = (
    "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest"
)
EVENTS_NS = "http://www.onvif.org/ver10/events/wsdl"
WSNT_NS = "http://docs.oasis-open.org/wsn/b-2"


def load_cameras(path: str | None) -> dict:
    """The HOMEOSTAT_CAMERAS TOML: per-camera host/username/password keyed
    by entity id. Unset env var: every camera unconfigured (each drops
    with a health event; the unit stays up)."""
    if not path:
        return {}
    return tomllib.loads(Path(path).read_text())


def resolve_host_port(conf: dict) -> tuple[str, int]:
    host = conf["host"]
    if ":" in host:
        h, _, p = host.rpartition(":")
        return h, int(p)
    return host, DEFAULT_PORT


def security_header(username: str, password: str) -> str:
    """WS-Security UsernameToken with PasswordDigest — what Tapo demands:
    Base64(SHA1(nonce + created + password))."""
    nonce = os.urandom(16)
    created = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    digest = base64.b64encode(
        hashlib.sha1(nonce + created.encode() + password.encode()).digest()
    ).decode()
    return (
        f'<wsse:Security xmlns:wsse="{WSSE}" xmlns:wsu="{WSU}">'
        "<wsse:UsernameToken>"
        f"<wsse:Username>{escape(username)}</wsse:Username>"
        f'<wsse:Password Type="{PASSWORD_DIGEST}">{digest}</wsse:Password>'
        f"<wsse:Nonce>{base64.b64encode(nonce).decode()}</wsse:Nonce>"
        f"<wsu:Created>{created}</wsu:Created>"
        "</wsse:UsernameToken>"
        "</wsse:Security>"
    )


def envelope(body: str, username: str, password: str) -> str:
    return (
        f'<s:Envelope xmlns:s="{SOAP_ENV}">'
        f"<s:Header>{security_header(username, password)}</s:Header>"
        f"<s:Body>{body}</s:Body>"
        "</s:Envelope>"
    )


class SoapError(Exception):
    """Any failure of a SOAP round trip: HTTP status, fault, bad XML."""


def fault_detail(text: str) -> str:
    """The fault's Reason/Text, or a bounded excerpt of whatever the
    camera actually said. A bare status code cannot distinguish which of
    four calls a camera objected to, or why."""
    with contextlib.suppress(ElementTree.ParseError):
        root = ElementTree.fromstring(text)
        reason = root.find(f".//{{{SOAP_ENV}}}Reason/{{{SOAP_ENV}}}Text")
        if reason is not None and (reason.text or "").strip():
            return reason.text.strip()[:FAULT_EXCERPT]
    return " ".join(text.split())[:FAULT_EXCERPT]


async def soap_call(
    http: aiohttp.ClientSession,
    url: str,
    body: str,
    username: str,
    password: str,
    op: str,
) -> ElementTree.Element:
    """`op` names the call in every error it can raise: a camera that
    accepts CreatePullPointSubscription and rejects Renew is a completely
    different problem from one that rejects the subscribe, and "HTTP 400"
    alone cannot tell them apart from outside the process."""
    try:
        async with http.post(
            url,
            data=envelope(body, username, password).encode(),
            headers={"Content-Type": "application/soap+xml; charset=utf-8"},
            timeout=aiohttp.ClientTimeout(total=HTTP_TIMEOUT_S),
        ) as response:
            text = await response.text()
            if response.status != 200:
                raise SoapError(f"{op}: HTTP {response.status}: {fault_detail(text)}")
    except aiohttp.ClientError as err:
        raise SoapError(f"{op}: {err}") from err
    except asyncio.TimeoutError as err:
        raise SoapError(f"{op}: timeout") from err
    try:
        root = ElementTree.fromstring(text)
    except ElementTree.ParseError as err:
        raise SoapError(f"{op}: unparseable response: {err}") from err
    if root.find(f".//{{{SOAP_ENV}}}Fault") is not None:
        raise SoapError(f"{op}: SOAP fault: {fault_detail(text)}")
    return root


def subscription_url(root: ElementTree.Element, base_url: str) -> str:
    """The SubscriptionReference address, with only its path and query
    trusted — the netloc stays the configured one."""
    address = root.find(".//{*}SubscriptionReference/{*}Address")
    if address is None or not (address.text or "").strip():
        raise SoapError("no subscription reference in response")
    base = urlsplit(base_url)
    ref = urlsplit(address.text.strip())
    return urlunsplit((base.scheme, base.netloc, ref.path, ref.query, ""))


def motion_values(root: ElementTree.Element):
    """(value, error) per motion notification in a PullMessages response:
    topic must mention Motion (CellMotionDetector/Motion, MotionAlarm —
    the C200's vocabulary), value from the IsMotion/State SimpleItem."""
    for message in root.iter(f"{{{WSNT_NS}}}NotificationMessage"):
        topic = message.find(".//{*}Topic")
        if topic is None or "Motion" not in (topic.text or ""):
            continue
        raw = None
        for item in message.iter():
            if item.tag.endswith("SimpleItem") and item.get("Name") in ("IsMotion", "State"):
                raw = item.get("Value")
        if raw == "true":
            yield True, None
        elif raw == "false":
            yield False, None
        else:
            yield None, f"unusable motion value {raw!r}"


async def run_camera(entity, conf: dict, session, http: aiohttp.ClientSession, stop: asyncio.Event) -> None:
    """One pull-point event stream for one camera: subscribe, long-poll,
    renew, forever; any fault recreates the subscription from scratch
    after a delay (one health event per down transition)."""
    motion_key = keys.state_key(entity.room, entity.name, "motion")
    available_key = keys.state_key(entity.room, entity.name, "available")
    try:
        host, port = resolve_host_port(conf)
        username, password = conf["username"], conf["password"]
    except (KeyError, ValueError) as err:
        # Unusable camera config: no amount of resubscribing fixes it.
        # The event is the trace; the camera reads unavailable.
        session.health_event("drop", reason="camera-misconfigured", camera=entity.name, error=str(err))
        session.put_json(available_key, False)
        return
    base_url = f"http://{host}:{port}/onvif/device_service"
    # Tri-state: None until the first subscription attempt settles, so the
    # first success and the first failure each publish availability once.
    up: bool | None = None
    # A camera whose subscribe SUCCEEDS and whose stream then fails
    # oscillates: up flips back to True each cycle, so "one event per down
    # transition" becomes one event per cycle. Count the consecutive
    # failures and back off, so a persistently broken camera is legible as
    # persistent instead of arriving as a steady drip.
    failures = 0
    delay = RESUBSCRIBE_DELAY_S
    # Whether this camera's SubscriptionManager answers. Set false by the
    # first Renew fault and stays false: re-asking every rotation would
    # fault every rotation.
    renews = True
    # The last motion value published, so a camera that re-asserts what it
    # already said does not republish it. Cameras differ on what a
    # notification means: a Tapo C200 sends MotionAlarm on every evaluation
    # tick, so one real episode arrives as hundreds of identical `true`s.
    # Kept across resubscription deliberately -- the camera's state did not
    # change because our subscription broke, and `motion` is documented to
    # stand through a loss rather than go false.
    last_motion: bool | None = None
    loop = asyncio.get_running_loop()

    while not stop.is_set():
        try:
            created = await soap_call(
                http,
                base_url,
                f'<tev:CreatePullPointSubscription xmlns:tev="{EVENTS_NS}">'
                f"<tev:InitialTerminationTime>{TERMINATION_TIME}</tev:InitialTerminationTime>"
                "</tev:CreatePullPointSubscription>",
                username,
                password,
                "CreatePullPointSubscription",
            )
            sub_url = subscription_url(created, base_url)
            created_at = loop.time()
            # A fresh subscription: believe Renew works until it says
            # otherwise AND a pull confirms the stream survived it.
            renew_fault: SoapError | None = None
            if up is not True:
                session.put_json(available_key, True)
                up = True
            while not stop.is_set():
                pulled = await soap_call(
                    http,
                    sub_url,
                    f'<tev:PullMessages xmlns:tev="{EVENTS_NS}">'
                    f"<tev:Timeout>{PULL_TIMEOUT}</tev:Timeout>"
                    "<tev:MessageLimit>100</tev:MessageLimit>"
                    "</tev:PullMessages>",
                    username,
                    password,
                    "PullMessages",
                )
                if renew_fault is not None:
                    # The pull above succeeded, so the stream was never
                    # lost: this firmware serves pull points and does not
                    # implement the SubscriptionManager. Stop renewing and
                    # rotate the subscription before it expires instead.
                    # Learned from behaviour, not from GetServiceCapabilities:
                    # no capability negotiation (see the module docstring).
                    renews = False
                    session.health_event(
                        "renew-unsupported", camera=entity.name, error=str(renew_fault)
                    )
                    renew_fault = None
                for value, error in motion_values(pulled):
                    if error is not None:
                        session.health_event(
                            "drop", reason="malformed-payload", camera=entity.name, error=error
                        )
                    elif value != last_motion:
                        session.put_json(motion_key, value)
                        last_motion = value
                if renews:
                    try:
                        await soap_call(
                            http,
                            sub_url,
                            f'<wsnt:Renew xmlns:wsnt="{WSNT_NS}">'
                            f"<wsnt:TerminationTime>{TERMINATION_TIME}</wsnt:TerminationTime>"
                            "</wsnt:Renew>",
                            username,
                            password,
                            "Renew",
                        )
                    except SoapError as err:
                        # A Renew fault has two causes and they need
                        # telling apart: firmware with no SubscriptionManager
                        # (the stream is fine), or a subscription that is
                        # genuinely gone (the stream is dead). Do not
                        # conclude yet — the NEXT pull decides, because it
                        # succeeds in the first case and fails in the
                        # second. Concluding here marks a camera whose
                        # subscription merely expired as permanently
                        # renew-less.
                        renew_fault = err
                # Progress is a COMPLETED round trip, not merely a
                # successful subscribe: a camera that accepts the
                # subscribe and refuses Renew would otherwise reset the
                # count every cycle and never back off at all.
                failures = 0
                delay = RESUBSCRIBE_DELAY_S
                if not renews and loop.time() - created_at >= RESUBSCRIBE_BEFORE_S:
                    # Best effort: a camera with a working SubscriptionManager
                    # is left clean, and the one that got us here refuses this
                    # too, which is exactly why the rotation exists.
                    with contextlib.suppress(SoapError):
                        await soap_call(
                            http,
                            sub_url,
                            f'<wsnt:Unsubscribe xmlns:wsnt="{WSNT_NS}"/>',
                            username,
                            password,
                            "Unsubscribe",
                        )
                    break
        except Exception as err:
            # ANY fault recreates the subscription after the delay — a
            # non-SOAP surprise (bad reply shape, a failed put) must not
            # silently end this camera's stream while the unit reads ready.
            # (CancelledError is BaseException and still cancels the task.)
            failures += 1
            if up is not False:
                session.health_event(
                    "drop",
                    reason="event-stream-lost",
                    camera=entity.name,
                    error=str(err),
                    consecutive_failures=failures,
                    retry_in_s=delay,
                )
                session.put_json(available_key, False)
                up = False
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(stop.wait(), timeout=delay)
            delay = min(delay * 2, RESUBSCRIBE_MAX_S)


async def serve(session, config, cameras_conf) -> None:
    loop = asyncio.get_running_loop()
    stop = asyncio.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)

    async with aiohttp.ClientSession() as http:
        tasks = []
        for entity in config.entities:
            conf = cameras_conf.get(entity.id)
            if not conf:
                session.health_event("drop", reason="camera-unconfigured", camera=entity.name)
                continue
            tasks.append(asyncio.create_task(run_camera(entity, conf, session, http, stop)))

        # Every configured camera has a subscription attempt in flight (its
        # own loop keeps trying); the unit is wired up.
        session.ready()

        await stop.wait()
        for task in tasks:
            task.cancel()
        for task in tasks:
            with contextlib.suppress(asyncio.CancelledError):
                await task


def main() -> None:
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    cameras_conf = load_cameras(os.environ.get(ENV_CAMERAS))

    session = homeostat.connect()
    try:
        asyncio.run(serve(session, config, cameras_conf))
    finally:
        session.close()


if __name__ == "__main__":
    main()
