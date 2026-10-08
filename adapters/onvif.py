# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "aiohttp>=3.12.14,<4",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""ONVIF adapter: camera motion events on the bus through Profile S pull points.

See docs/design.md#cameras. The adapter handles the event plane only:
on-camera detections become scalar aspects, currently `motion` (a bool)
at home/state/{room}/{entity}/motion. Video goes through go2rtc
(adapters/go2rtc.py). There is no PTZ, imaging service or capability
negotiation. On Tapo cameras, person detection is app-only and not
exposed over ONVIF, and pan/tilt and privacy mode need the vendor API;
an adapter for those would take the cameras over (docs/adapters.md,
Files).

Binding: an entity file's `id` is the camera's key in the
HOMEOSTAT_CAMERAS file.

Configuration: HOMEOSTAT_CAMERAS names a TOML file outside the repo with,
per camera, `host` (optionally `host:port`; the default port is Tapo's
ONVIF port 2020), `username` and `password`. These are the camera-account
credentials created in the vendor app (on Tapo, with third-party
compatibility enabled). A camera with no entry drops with
`camera-unconfigured` and is skipped. An entry missing a field drops with
`camera-misconfigured` and the camera reads unavailable. Other cameras
are unaffected either way.

State: `motion`, published when it changes, and `available`, true while
a pull-point subscription works (`run_camera`).

Health events: `drop` (camera-unconfigured, camera-misconfigured,
malformed-payload), event-stream-lost and renew-unsupported.

The SOAP layer is hand-written, because an ONVIF/WS-* client library
would be the largest dependency in the tree for four calls:
CreatePullPointSubscription, PullMessages (a long poll), Renew and
Unsubscribe, each with a WS-Security UsernameToken digest header.
"""

import asyncio
import base64
import contextlib
import hashlib
import os
import signal
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit
from xml.etree import ElementTree
from xml.sax.saxutils import escape

import aiohttp
import homeostat
import tomllib
from homeostat import house, keys

ENV_CAMERAS = "HOMEOSTAT_CAMERAS"
DEFAULT_PORT = 2020  # Tapo's ONVIF service port; override per camera with host:port
PULL_TIMEOUT = "PT10S"
TERMINATION_TIME = "PT60S"
TERMINATION_S = 60
# Re-create a subscription this long after it was created, for cameras
# with no working Renew. The old one lingers until it times out, so this
# also bounds the overlap: at 40 s against a PT60S termination, at most
# two per camera are live at once, well under the MaxPullPoints these
# firmwares advertise.
RESUBSCRIBE_BEFORE_S = 40
RESUBSCRIBE_DELAY_S = 5
# A camera that rejects a call will reject it again, so back off toward
# RESUBSCRIBE_MAX_S instead of retrying a live camera at a fixed rate. A
# completed round trip resets it.
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
    """Load the HOMEOSTAT_CAMERAS TOML: per-camera host/username/password keyed by entity id.

    Unset env var: every camera unconfigured (each drops with a health
    event; the unit stays up).
    """
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
    """Build a WS-Security UsernameToken with PasswordDigest, which Tapo requires.

    The digest is Base64(SHA1(nonce + created + password)).
    """
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
    """Return the fault's Reason/Text, or a bounded excerpt of whatever the camera actually said.

    A bare status code does not say which of four calls a camera objected
    to, or why.
    """
    with contextlib.suppress(ElementTree.ParseError):
        root = ElementTree.fromstring(text)
        reason = root.find(f".//{{{SOAP_ENV}}}Reason/{{{SOAP_ENV}}}Text")
        if reason is not None and (reason.text or "").strip():
            return reason.text.strip()[:FAULT_EXCERPT]
    return " ".join(text.split())[:FAULT_EXCERPT]


# A PullMessages reply is a few KB; a misbehaving camera must not pin
# the unit's memory on an oversized one.
MAX_RESPONSE_BYTES = 1024 * 1024
# How much of the body one read takes. Only a buffer size: the cap above
# is what bounds memory, and it is checked after every chunk.
RESPONSE_CHUNK_BYTES = 64 * 1024


async def soap_call(
    http: aiohttp.ClientSession,
    url: str,
    body: str,
    username: str,
    password: str,
    op: str,
) -> ElementTree.Element:
    """Make one SOAP call and return the reply's root, raising SoapError on any failure.

    `op` names the call in every error it can raise, and the error carries
    the fault's reason. A camera that accepts CreatePullPointSubscription
    and rejects Renew is a different problem from one that rejects the
    subscribe, and "HTTP 400" alone cannot tell them apart from outside
    the process.
    """
    try:
        async with http.post(
            url,
            data=envelope(body, username, password).encode(),
            headers={"Content-Type": "application/soap+xml; charset=utf-8"},
            timeout=aiohttp.ClientTimeout(total=HTTP_TIMEOUT_S),
        ) as response:
            # Read until EOF. `content.read(n)` returns whatever is
            # buffered, up to n, which for a chunked reply is the first
            # chunk. A single read then truncates the document and every
            # parse fails with "unclosed token". Cameras stream their
            # replies and aiohttp's test responses do not, so a one-shot
            # read passes the tests and fails against a camera. The size cap
            # is checked after each chunk, so it does not depend on how the
            # body is framed.
            raw = bytearray()
            async for chunk in response.content.iter_chunked(RESPONSE_CHUNK_BYTES):
                raw += chunk
                if len(raw) > MAX_RESPONSE_BYTES:
                    raise SoapError(f"{op}: response exceeds {MAX_RESPONSE_BYTES} bytes")
            text = bytes(raw).decode(response.get_encoding(), errors="replace")
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
    """Return the SubscriptionReference address, with only its path and query trusted.

    The netloc stays the configured one. The camera may return an
    unroutable host (NAT, container namespaces), and a Tapo tested in the
    field advertises a per-subscription port (1024, 1025, ...) that
    nothing can connect to. Without the rewrite every call after the
    subscribe would time out.
    """
    address = root.find(".//{*}SubscriptionReference/{*}Address")
    if address is None or not (address.text or "").strip():
        raise SoapError("no subscription reference in response")
    base = urlsplit(base_url)
    ref = urlsplit(address.text.strip())
    return urlunsplit((base.scheme, base.netloc, ref.path, ref.query, ""))


def motion_values(root: ElementTree.Element):
    """Yield (value, error) per motion notification in a PullMessages response.

    The topic must mention Motion (CellMotionDetector/Motion, MotionAlarm —
    the C200's vocabulary); the value comes from the IsMotion/State
    SimpleItem.
    """
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
    """Run one pull-point event stream for one camera: subscribe, long-poll, renew, forever.

    Any fault on the event stream (HTTP error, SOAP fault, timeout,
    unparseable envelope) tears the subscription down and recreates it
    after a delay, because Tapo firmware has broken pull-point
    subscriptions before (the 1.3.6 regression). Each down transition
    reports one `event-stream-lost` with the consecutive-failure count, and
    the delay doubles from RESUBSCRIBE_DELAY_S up to RESUBSCRIBE_MAX_S.

    A working subscription publishes `available = true` and its loss
    `false`. `motion` keeps its last value through a loss.

    `motion` is published when it changes, not per notification. A Tapo
    C200 sends MotionAlarm on every evaluation tick: one real episode at
    the house this was written for arrived as 417 identical `true`s in 56
    seconds. Comparing with the last published value makes the camera
    behave like the event-driven adapters, whose devices speak only on
    change. A notification with an unusable value drops with
    `malformed-payload` and the stream continues.

    Renew and Unsubscribe are WS-BaseNotification SubscriptionManager
    operations, and firmware that serves pull points may implement
    neither. The same Tapo answers CreatePullPointSubscription and
    PullMessages with 200, Renew and Unsubscribe with 400, and its
    GetServiceCapabilities reports no SubscriptionManager. After a Renew
    fault the next pull decides: if it succeeds, the camera has no
    SubscriptionManager, so it reports `renew-unsupported`, stops renewing,
    and rotates the subscription every RESUBSCRIBE_BEFORE_S, unsubscribing
    the old one best effort. Availability does not change, since nothing
    was lost. If the pull fails, the ordinary loss path runs. This is
    learned from behaviour, since the adapter does no capability
    negotiation.
    """
    motion_key = keys.state_key(entity.room, entity.name, "motion")
    available_key = keys.state_key(entity.room, entity.name, "available")
    try:
        host, port = resolve_host_port(conf)
        username, password = conf["username"], conf["password"]
    except (KeyError, ValueError) as err:
        # Unusable camera config, which resubscribing cannot fix. Report
        # it and mark the camera unavailable.
        session.health_event("drop", reason="camera-misconfigured", camera=entity.name, error=str(err))
        session.put_json(available_key, False)
        return
    base_url = f"http://{host}:{port}/onvif/device_service"
    # Tri-state: None until the first subscription attempt settles, so the
    # first success and the first failure each publish availability once.
    up: bool | None = None
    # A camera whose subscribe succeeds and whose stream then fails
    # oscillates: `up` flips back to True each cycle, so "one event per down
    # transition" becomes one event per cycle. Counting consecutive
    # failures and backing off shows a persistently broken camera as such.
    failures = 0
    delay = RESUBSCRIBE_DELAY_S
    # Whether this camera's SubscriptionManager answers. Once false it
    # stays false, since asking again every rotation would fault every
    # rotation.
    renews = True
    # The last motion value published, so a repeated value is not
    # republished. Kept across resubscription: the camera's state did not
    # change because the subscription broke, and `motion` keeps its value
    # through a loss.
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
            # A fresh subscription. Renew is assumed to work until it
            # faults and a pull then confirms the stream survived.
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
                    # The pull above succeeded, so the stream was not
                    # lost: this firmware serves pull points without the
                    # SubscriptionManager. Stop renewing and rotate the
                    # subscription before it expires.
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
                        # Either the firmware has no SubscriptionManager
                        # (the stream is fine) or the subscription is gone
                        # (the stream is dead). The next pull decides.
                        # Deciding here would mark a camera whose
                        # subscription merely expired as renew-less for
                        # good.
                        renew_fault = err
                # Reset only after a completed round trip. Resetting on a
                # successful subscribe would let a camera that accepts the
                # subscribe and refuses Renew reset the count every cycle
                # and never back off.
                failures = 0
                delay = RESUBSCRIBE_DELAY_S
                if not renews and loop.time() - created_at >= RESUBSCRIBE_BEFORE_S:
                    # Best effort. A camera with a working
                    # SubscriptionManager is left clean; the cameras that
                    # need rotation refuse this too.
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
            # Any exception recreates the subscription after the delay. A
            # non-SOAP error (bad reply shape, a failed put) must not end
            # this camera's stream while the unit reads ready.
            # CancelledError is a BaseException and still cancels the task.
            failures += 1
            if up is not False:
                session.health_event(
                    "event-stream-lost",
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

        # Every configured camera has a subscription attempt in flight, and
        # each camera's loop keeps retrying on its own.
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
