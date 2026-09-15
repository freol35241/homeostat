# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "aiohttp>=3.9,<4",
# ]
# ///
"""A minimal, honest ONVIF Profile S pull-point event service, for the
onvif adapter's integration tests (tests/onvif.rs; see docs/design.md,
"Cameras (settled 2026-07-19)").

Speaks the real SOAP shapes the adapter sends: CreatePullPointSubscription
(returning a SubscriptionReference address — deliberately with a WRONG,
unroutable host, the Tapo/NAT quirk the adapter must survive by keeping
the configured netloc), PullMessages (a genuine long poll against a
per-subscription queue), and Renew. Every SOAP request must carry a valid
WS-Security UsernameToken PasswordDigest (Base64(SHA1(nonce + created +
password))) — a bad or missing digest gets 401, which is how the tests
know the adapter authenticates.

Test control rides plain HTTP on the same port, out of the SOAP path:

  POST /control/trigger?value=true|false|<garbage>  -> queue a motion
       notification carrying that IsMotion value on every live
       subscription (garbage exercises the malformed-payload drop)
  POST /control/break -> invalidate every subscription: the next
       PullMessages/Renew on an old address gets a SOAP fault, forcing
       the adapter to resubscribe
  POST /control/chunked -> send every later SOAP reply with
       Transfer-Encoding: chunked, split across two TCP writes. Real
       cameras stream their replies; aiohttp's `web.Response` does not,
       so without this the adapter is only ever tested against a body
       that arrives in one piece.
"""

import argparse
import asyncio
import base64
import hashlib
import itertools
from datetime import datetime, timezone
from xml.etree import ElementTree

from aiohttp import web

SOAP_ENV = "http://www.w3.org/2003/05/soap-envelope"
EVENTS_NS = "http://www.onvif.org/ver10/events/wsdl"
WSNT_NS = "http://docs.oasis-open.org/wsn/b-2"
ONVIF_SCHEMA = "http://www.onvif.org/ver10/schema"


def envelope(body: str) -> str:
    return (
        f'<s:Envelope xmlns:s="{SOAP_ENV}" xmlns:tev="{EVENTS_NS}"'
        f' xmlns:wsnt="{WSNT_NS}" xmlns:tt="{ONVIF_SCHEMA}">'
        f"<s:Body>{body}</s:Body></s:Envelope>"
    )


def soap(body: str) -> web.Response:
    return web.Response(text=envelope(body), content_type="application/soap+xml")


# Where the chunked reply below is cut. Anywhere inside the document does;
# this lands in the opening tag, so a client that stops at the first chunk
# fails to parse rather than quietly getting a shorter document.
CHUNK_AT = 24


async def soap_chunked(request: web.Request, body: str) -> web.StreamResponse:
    """The same envelope, chunked across two TCP writes with a pause
    between them — what a camera's own HTTP stack does, and what
    `web.Response` cannot express. A client reading "up to n bytes" once
    sees only the first chunk."""
    # charset included, as web.Response(text=...) does it: the client reads
    # the encoding off this header, and omitting it changes more than the
    # framing, which is the one thing this is meant to vary.
    response = web.StreamResponse(
        headers={"Content-Type": "application/soap+xml; charset=utf-8"}
    )
    response.enable_chunked_encoding()
    await response.prepare(request)
    raw = envelope(body).encode()
    await response.write(raw[:CHUNK_AT])
    await asyncio.sleep(0.05)
    await response.write(raw[CHUNK_AT:])
    await response.write_eof()
    return response


def fault() -> web.Response:
    return soap(f'<s:Fault xmlns:s="{SOAP_ENV}"><s:Code></s:Code></s:Fault>')


def now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def notification(value: str) -> str:
    return (
        "<wsnt:NotificationMessage>"
        '<wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">'
        "tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic>"
        f'<wsnt:Message><tt:Message UtcTime="{now()}">'
        f'<tt:Data><tt:SimpleItem Name="IsMotion" Value="{value}"/></tt:Data>'
        "</tt:Message></wsnt:Message>"
        "</wsnt:NotificationMessage>"
    )


class FakeCamera:
    def __init__(self, username: str, password: str):
        self.username = username
        self.password = password
        self.subscriptions: dict[str, asyncio.Queue] = {}
        self.ids = itertools.count()
        # VP52's shape: the subscribe is accepted and the long poll runs,
        # then Renew is refused. A bare status code cannot tell that apart
        # from a refused subscribe.
        self.reject_renew = False
        self.break_on_renew = False
        self.created = 0
        # Whether SOAP replies stream (see soap_chunked).
        self.chunked = False

    async def reply(self, request: web.Request, body: str) -> web.StreamResponse:
        """One SOAP reply, chunked or not as /control/chunked says."""
        if self.chunked:
            return await soap_chunked(request, body)
        return soap(body)

    def authenticated(self, root: ElementTree.Element) -> bool:
        token = root.find(".//{*}UsernameToken")
        if token is None:
            return False
        username = token.findtext("{*}Username", "")
        digest = token.findtext("{*}Password", "")
        nonce = token.findtext("{*}Nonce", "")
        created = token.findtext("{*}Created", "")
        try:
            expected = base64.b64encode(
                hashlib.sha1(
                    base64.b64decode(nonce) + created.encode() + self.password.encode()
                ).digest()
            ).decode()
        except ValueError:
            return False
        return username == self.username and digest == expected

    async def device_service(self, request: web.Request) -> web.StreamResponse:
        root = ElementTree.fromstring(await request.text())
        if not self.authenticated(root):
            return web.Response(status=401)
        if root.find(f".//{{{EVENTS_NS}}}CreatePullPointSubscription") is None:
            return fault()
        sub_id = f"sub_{next(self.ids)}"
        self.subscriptions[sub_id] = asyncio.Queue()
        self.created += 1
        # A deliberately unroutable netloc: the adapter must keep the
        # configured host and trust only the path.
        return await self.reply(
            request,
            "<tev:CreatePullPointSubscriptionResponse>"
            "<tev:SubscriptionReference>"
            f"<wsnt:Address>http://192.0.2.1:9999/onvif/{sub_id}</wsnt:Address>"
            "</tev:SubscriptionReference>"
            f"<wsnt:CurrentTime>{now()}</wsnt:CurrentTime>"
            f"<wsnt:TerminationTime>{now()}</wsnt:TerminationTime>"
            "</tev:CreatePullPointSubscriptionResponse>",
        )

    async def subscription(self, request: web.Request) -> web.StreamResponse:
        root = ElementTree.fromstring(await request.text())
        if not self.authenticated(root):
            return web.Response(status=401)
        queue = self.subscriptions.get(request.match_info["sub_id"])
        if queue is None:
            return fault()
        if root.find(f".//{{{WSNT_NS}}}Renew") is not None:
            if self.break_on_renew:
                # The race CI caught: the subscription really is gone, and
                # Renew is the call that discovers it. The next pull must
                # fail too, which is how the adapter tells this apart from
                # firmware that simply has no SubscriptionManager.
                self.subscriptions.clear()
                return fault()
            if self.reject_renew:
                return web.Response(
                    status=400,
                    text=(
                        f'<s:Envelope xmlns:s="{SOAP_ENV}"><s:Body><s:Fault>'
                        "<s:Reason><s:Text>ter:InvalidArgVal renew refused</s:Text>"
                        "</s:Reason></s:Fault></s:Body></s:Envelope>"
                    ),
                    content_type="application/soap+xml",
                )
            return await self.reply(
                request,
                f"<wsnt:RenewResponse><wsnt:TerminationTime>{now()}</wsnt:TerminationTime>"
                "</wsnt:RenewResponse>",
            )
        if root.find(f".//{{{EVENTS_NS}}}PullMessages") is None:
            return fault()
        messages = []
        try:
            messages.append(await asyncio.wait_for(queue.get(), timeout=5))
            while not queue.empty():
                messages.append(queue.get_nowait())
        except asyncio.TimeoutError:
            pass
        return await self.reply(
            request,
            "<tev:PullMessagesResponse>"
            f"<tev:CurrentTime>{now()}</tev:CurrentTime>"
            f"<tev:TerminationTime>{now()}</tev:TerminationTime>"
            f"{''.join(notification(v) for v in messages)}"
            "</tev:PullMessagesResponse>",
        )

    async def trigger(self, request: web.Request) -> web.Response:
        value = request.query.get("value", "true")
        for queue in self.subscriptions.values():
            queue.put_nowait(value)
        return web.json_response({"subscriptions": len(self.subscriptions)})

    async def chunk_replies(self, request: web.Request) -> web.Response:
        self.chunked = True
        return web.json_response({"chunked": True})

    async def break_on_renews(self, request: web.Request) -> web.Response:
        self.break_on_renew = True
        return web.json_response({"break_on_renew": True})

    async def stats(self, request: web.Request) -> web.Response:
        return web.json_response({"created": self.created})

    async def reject_renews(self, request: web.Request) -> web.Response:
        self.reject_renew = True
        return web.json_response({"reject_renew": True})

    async def break_subscriptions(self, request: web.Request) -> web.Response:
        count = len(self.subscriptions)
        self.subscriptions.clear()
        return web.json_response({"broken": count})


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--username", default="homeostat")
    parser.add_argument("--password", default="secret123")
    args = parser.parse_args()

    camera = FakeCamera(args.username, args.password)
    app = web.Application()
    app.router.add_post("/onvif/device_service", camera.device_service)
    app.router.add_post("/onvif/{sub_id}", camera.subscription)
    app.router.add_post("/control/trigger", camera.trigger)
    app.router.add_post("/control/break", camera.break_subscriptions)
    app.router.add_post("/control/reject-renew", camera.reject_renews)
    app.router.add_post("/control/stats", camera.stats)
    app.router.add_post("/control/break-on-renew", camera.break_on_renews)
    app.router.add_post("/control/chunked", camera.chunk_replies)
    web.run_app(app, host="127.0.0.1", port=args.port, print=None)


if __name__ == "__main__":
    main()
