# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "aiohttp>=3.9,<4",
# ]
# ///
"""A minimal, honest OpenWrt ubus endpoint, for the openwrt adapter's
integration tests (tests/openwrt.rs; see docs/design.md, "Network
presence and connectivity (settled 2026-07-25)").

Speaks the slice of the ubus JSON-RPC surface the adapter touches: POST
/ubus with `session.login` (rpcd credential check, real session id
enforcement on every later call — status 6 on a bad sid, like rpcd),
`list` for hostapd.* object names, `network.interface dump`,
`hostapd.X get_clients`, and `luci.wireguard getWireguardStatus`.

The modelled router has a WAN (dhcp), a WireGuard tunnel wg0, an
OpenVPN-style tunnel vpn0 (proto none on a tun device — netifd's view of
a service-managed tunnel), and one hostapd BSS whose associated stations
the tests mutate.

Test control (plain HTTP, out of the JSON-RPC path):
  - POST /control/wan?up=true|false
  - POST /control/tunnel?iface=vpn0&up=true|false
  - POST /control/wg?age=<seconds>          handshake age of wg0's peer
  - POST /control/station?mac=..&present=true|false
  - POST /control/break                     every /ubus call becomes HTTP 500
  - POST /control/restore
"""

import argparse
import secrets
import time

from aiohttp import web

STATE = {
    "wan_up": True,
    "vpn0_up": True,
    "wg_handshake_age": 5,
    "stations": set(),
    "broken": False,
}


def make_app(username: str, password: str) -> web.Application:
    sid = secrets.token_hex(16)

    def interfaces() -> list[dict]:
        return [
            {"interface": "lan", "up": True, "proto": "static", "device": "br-lan"},
            {"interface": "wan", "up": STATE["wan_up"], "proto": "dhcp", "device": "eth1"},
            {"interface": "wg0", "up": True, "proto": "wireguard", "device": "wg0"},
            {"interface": "vpn0", "up": STATE["vpn0_up"], "proto": "none", "device": "tun0"},
        ]

    def call(req_sid: str, obj: str, method: str, args: dict):
        """One ubus invocation -> the JSON-RPC `result` array."""
        if obj == "session" and method == "login":
            if args.get("username") == username and args.get("password") == password:
                return [0, {"ubus_rpc_session": sid}]
            return [6]
        if req_sid != sid:
            return [6]
        if obj == "network.interface" and method == "dump":
            return [0, {"interface": interfaces()}]
        if obj == "hostapd.phy0-ap0" and method == "get_clients":
            return [0, {"clients": {mac: {"auth": True} for mac in STATE["stations"]}}]
        if obj == "luci.wireguard" and method == "getWireguardStatus":
            handshake = int(time.time()) - STATE["wg_handshake_age"]
            return [0, {"wg0": {"peers": [{"public_key": "pk", "latest_handshake": handshake}]}}]
        return [2]  # UBUS_STATUS_INVALID_COMMAND

    async def ubus(request: web.Request) -> web.Response:
        if STATE["broken"]:
            return web.Response(status=500, text="internal error")
        body = await request.json()
        method, params = body.get("method"), body.get("params", [])
        if method == "list":
            result = {"hostapd.phy0-ap0": {}} if params[0] == sid else {}
        elif method == "call":
            req_sid, obj, obj_method = params[0], params[1], params[2]
            args = params[3] if len(params) > 3 else {}
            result = call(req_sid, obj, obj_method, args)
        else:
            return web.json_response(
                {"jsonrpc": "2.0", "id": body.get("id"), "error": {"code": -32601}}
            )
        return web.json_response({"jsonrpc": "2.0", "id": body.get("id"), "result": result})

    async def control(request: web.Request) -> web.Response:
        action = request.match_info["action"]
        q = request.query
        if action == "wan":
            STATE["wan_up"] = q["up"] == "true"
        elif action == "tunnel":
            STATE["vpn0_up"] = q["up"] == "true"
        elif action == "wg":
            STATE["wg_handshake_age"] = int(q["age"])
        elif action == "station":
            mac = q["mac"].lower()
            if q["present"] == "true":
                STATE["stations"].add(mac)
            else:
                STATE["stations"].discard(mac)
        elif action == "break":
            STATE["broken"] = True
        elif action == "restore":
            STATE["broken"] = False
        else:
            return web.Response(status=404)
        return web.Response(text="ok")

    app = web.Application()
    app.router.add_post("/ubus", ubus)
    app.router.add_post("/control/{action}", control)
    return app


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--username", required=True)
    parser.add_argument("--password", required=True)
    args = parser.parse_args()
    web.run_app(
        make_app(args.username, args.password),
        host="127.0.0.1",
        port=args.port,
        print=None,
    )


if __name__ == "__main__":
    main()
