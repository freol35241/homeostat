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
tears the subscription down and recreates it from scratch after a short
delay, with one "event-stream-lost" health event per down transition,
never a crash. A notification that parses but carries an unusable value
drops with a "malformed-payload" health event and the stream continues.

The camera may return a subscription address with an unroutable host (NAT,
container namespaces); only its path and query are trusted — the netloc
stays the configured one.
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
from xml.etree import ElementTree

import aiohttp

import homeostat
from homeostat import house, keys

ENV_CAMERAS = "HOMEOSTAT_CAMERAS"
DEFAULT_PORT = 2020  # Tapo's ONVIF service port; override per camera with host:port
PULL_TIMEOUT = "PT10S"
TERMINATION_TIME = "PT60S"
RESUBSCRIBE_DELAY_S = 5
HTTP_TIMEOUT_S = 30  # must exceed the PT10S long poll

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
        f"<wsse:Username>{username}</wsse:Username>"
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


async def soap_call(http: aiohttp.ClientSession, url: str, body: str, username: str, password: str) -> ElementTree.Element:
    try:
        async with http.post(
            url,
            data=envelope(body, username, password).encode(),
            headers={"Content-Type": "application/soap+xml; charset=utf-8"},
            timeout=aiohttp.ClientTimeout(total=HTTP_TIMEOUT_S),
        ) as response:
            text = await response.text()
            if response.status != 200:
                raise SoapError(f"HTTP {response.status}")
    except aiohttp.ClientError as err:
        raise SoapError(str(err)) from err
    except asyncio.TimeoutError as err:
        raise SoapError("timeout") from err
    try:
        root = ElementTree.fromstring(text)
    except ElementTree.ParseError as err:
        raise SoapError(f"unparseable response: {err}") from err
    if root.find(f".//{{{SOAP_ENV}}}Fault") is not None:
        raise SoapError("SOAP fault")
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
    host, port = resolve_host_port(conf)
    base_url = f"http://{host}:{port}/onvif/device_service"
    username, password = conf["username"], conf["password"]
    motion_key = keys.state_key(entity.room, entity.name, "motion")
    up = True  # one event per down transition, not per retry

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
            )
            sub_url = subscription_url(created, base_url)
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
                )
                for value, error in motion_values(pulled):
                    if error is not None:
                        session.health_event(
                            "drop", reason="malformed-payload", camera=entity.name, error=error
                        )
                    else:
                        session.put_json(motion_key, value)
                await soap_call(
                    http,
                    sub_url,
                    f'<wsnt:Renew xmlns:wsnt="{WSNT_NS}">'
                    f"<wsnt:TerminationTime>{TERMINATION_TIME}</wsnt:TerminationTime>"
                    "</wsnt:Renew>",
                    username,
                    password,
                )
        except SoapError as err:
            if up:
                session.health_event(
                    "drop", reason="event-stream-lost", camera=entity.name, error=str(err)
                )
                up = False
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(stop.wait(), timeout=RESUBSCRIBE_DELAY_S)


async def serve(unit, session, config, cameras_conf) -> None:
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
        asyncio.run(serve(unit, session, config, cameras_conf))
    finally:
        session.close()


if __name__ == "__main__":
    main()
