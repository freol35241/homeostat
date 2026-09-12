# The companion app protocol

The wire contract between a family phone running the homeostat app
([`homeostat-app`](https://github.com/freol35241/homeostat-app)) and the
`companion` adapter (`adapters/companion.py`). It is normative: this file
and the adapter are changed together, in this repo, and the app
implements against it. A change the app needs is an issue here, not a
decision on the phone side.

[docs/design.md](design.md), "The companion app", holds the reasoning —
why the broker and not ntfy, why the phone never touches the bus, why
presence rather than a map. [docs/adapters.md](adapters.md) holds the
rules the adapter itself obeys.

## The shape

The phone is a device. It speaks MQTT to the broker the house already
runs, over the WireGuard tunnel; it never touches the Zenoh bus, and it
never reads house state. It says two things — where the person is, and
that they saw the notification — and hears one: the notification.

Each phone owns one topic subtree and, in the house repo, two entity
files: a `person` and a `notifier`. The adapter binds them by `id`, which
is the two topic segments naming the subtree and the face.

## Connection

- **Endpoint.** The broker, reachable over the tunnel. The app assumes it
  is on the house network; bringing the tunnel up is the WireGuard app's
  job, not this app's.
- **Credentials.** One MQTT username and password per phone, from the
  broker's password file. The phone's identity IS this credential: there
  are no accounts, and the WireGuard peer is a network identity the
  broker cannot see.
- **Client id.** `companion-{phone}`, stable for the life of the install.
- **A persistent session, always.** MQTT 3.1.1 `clean_session = false`,
  or MQTT 5 with a session expiry longer than any outage worth surviving.
  This is the queue that holds an alert for a phone in a tunnel, and it
  is the reason the house uses the broker rather than a socket of its
  own. An app that connects with a clean session silently loses every
  notification sent while it was away.
- **Keepalive.** Minutes, not seconds. The tunnel keeps itself alive;
  under Doze the app needs a battery-optimization exemption, backoff and
  resubscribe on every reconnect.

## Topics

`{base}` is the base topic, `companion` unless the adapter's endpoint
names another (`mqtt://host:1883/other/prefix`). `{phone}` is the subtree
segment for one phone — `alice` — and is the first segment of both of
that phone's entity ids.

| Topic | Direction | QoS | Retain | Payload |
|---|---|---|---|---|
| `{base}/{phone}/available` | app → house | 1 | **yes** | `true` on connect; `false` as the last will |
| `{base}/{phone}/person/presence` | app → house | 1 | no | `true` / `false` |
| `{base}/{phone}/person/position` | app → house | 1 | no | a fix object, below |
| `{base}/{phone}/notifier/ack` | app → house | 1 | no | epoch seconds, a bare number |
| `{base}/{phone}/notifier/message` | house → app | 1 | no | a notification object, below |
| `{base}/{phone}/notifier/alert` | house → app | 1 | no | the same, the urgent channel |

Every payload is JSON. The three scalar topics carry a bare JSON scalar,
not an object wrapping it.

The app publishes nothing else under its subtree and subscribes to
nothing outside it. The adapter subscribes to exactly the four upward
topics; anything else a phone publishes is invisible to the house.

### `available` — the phone's own liveness

Retained `true` published on connect, and a retained `false` registered
as the connection's last will, so the broker publishes it when the
session drops. The adapter forwards it to `available` on BOTH of that
phone's entities, on transition only.

MQTT allows exactly one last will per connection, which is why this topic
addresses the phone rather than one of its faces. `delivered` only says
the broker took the message; `available` is the honest answer to whether
the phone can be reached at all, and an automation deciding whether to
escalate an unacknowledged alert reads it.

### `presence` — the geofence transition

`true` on entering the `home` geofence, `false` on leaving. The app
registers ONE geofence and uses the platform's Geofencing API, which the
OS runs at near-zero battery cost; it does not run a location loop.

The house fuses this with the router's WiFi sightings elsewhere; the
phone's job is `away` and `approaching`, not `at home`.

### `position` — the opt-in fix

Off by default, per phone. When on:

```json
{"lat": 59.33, "lon": 18.06, "accuracy": 12, "battery": 87, "fixed_at": 1752600000}
```

`lat` and `lon` are required numbers; `accuracy` (metres), `battery`
(percent) and `fixed_at` (epoch seconds) are optional and omitted rather
than nulled when the fix does not carry them. The adapter fans the object
out to scalar aspects, so the recorder gives trails for free; it publishes
nothing for an absent field, and never invents one.

A fix without `lat` or `lon`, or with a non-numeric one, drops with
`malformed-payload`.

### `ack` — the far-end receipt

A bare epoch-seconds number, published when the person dismissed or
opened the notification, becoming `acknowledged` on the notifier entity.
This is the receipt no delivery service can give: `delivered` means the
broker accepted the message, `acknowledged` means a human saw it.

The app decides nothing about escalation. Whether an unacknowledged
`alert` escalates is an automation's policy over the aspect.

### `message` and `alert` — what the house says

```json
{"text": "Motion in the hall and nobody home", "actor": "intrusion", "sent_at": 1752600000.12}
```

`text` is the notification body, `actor` the unit that sent it (show it
as the title, so the phone says who spoke), `sent_at` the adapter's
publish time in epoch seconds.

Two topics, not one payload field with a severity in it: the severity
split is structural everywhere else in the house — separately granted,
separately policed, separately recorded — and it stays structural on the
wire. The app maps them onto two notification channels, and `alert` is
the one with bypass-DND, which needs notification-policy access granted
once.

**Never retained.** A retained alert would fire again on every reconnect.
Nothing here is a state to be caught up on; the persistent session is
what makes an offline phone get the message, exactly once, when it
returns.

## Provisioning a phone

Three things, all text:

1. **Two entity files** in the house repo, under the companion adapter's
   entities directory — `alice.toml` (capability `person`, id
   `alice/person`) and `alice_phone.toml` (capability `notifier`, id
   `alice/notifier`), both room `person`. `plan` renders the result; the
   tests' fixture house is the worked example.
2. **A broker user and ACL**, one block per phone:

   ```
   user alice-phone
   topic write companion/alice/available
   topic write companion/alice/person/presence
   topic write companion/alice/person/position
   topic write companion/alice/notifier/ack
   topic read  companion/alice/notifier/message
   topic read  companion/alice/notifier/alert
   ```

   Write access is listed per topic rather than as `companion/alice/#`:
   a phone has no business publishing its own notifications, and the ACL
   is what stops one phone writing another's subtree.
3. **The app's configuration**, handed over as text the house repo can
   render as a QR code — the file itself, not a link to it:

   ```toml
   broker = "mqtt://10.0.0.1:1883/companion"
   phone = "alice"
   username = "alice-phone"
   password = "..."
   ```

   `broker` is the endpoint, its path the base topic when the house runs
   a non-default one. `phone` is the subtree segment. Config as text,
   like everything else; an install is a scan, and a re-provision is
   another scan.

## Known limits, deliberately

- **The adapter's own MQTT session is clean, not persistent.** An `ack`
  published while the adapter was down is lost rather than invented, and
  the app cannot tell. Presence and position are live signals and the
  house's state mirror holds the last value, so a restart loses nothing
  else.
- **`delivered` is the broker's QoS 1 ack**, not the phone's. The pair
  `delivered` / `acknowledged` is the whole point: one says the delivery
  path took it, the other that a person saw it.
- **No rate floor in the adapter**, unlike ntfy: there is no third-party
  server to protect, and the cooldown that is house policy lives in the
  automation.
- **Android only.** Push on iOS goes through Apple's service, a cloud
  dependency in the alerting path that the design rejects.
