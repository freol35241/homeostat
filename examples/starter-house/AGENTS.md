# Working on this house

This repository is a homeostat house: the whole configuration of a home,
as text. If you are an agent asked to change something here, this is how
it works and what you may do. People are welcome to read it too.

## What is here

| Path | What it is |
|---|---|
| `units/*.toml` | One manifest per unit (an adapter, an automation, a service): what it publishes and subscribes to, its parameters, how it runs. |
| `units/*.py` | Unit code. The adapters (`zigbee.py`, `esphome.py`, `ivt490.py`, `owntracks.py`, `clock.py`, `recorder.py`, `arbiter.py`, `dashboard.py`, `dashboard.html`, `assets/`) are copies of a homeostat release: do not edit them. `evening_lights.py` is the house's own automation, yours to change. |
| `entities/<adapter>/*.toml` | One file per device: its capability, room and binding. |
| `zones.toml` | The rooms, grouped into zones. |
| `dashboard.toml` | The family dashboard's views, as widgets. |
| `docker-compose.yml`, `mosquitto.*` | How the stack runs. Credentials are never in the repo (`.env`, `mosquitto.passwd` are gitignored). |

## The loop

1. Edit the files.
2. Check the change against the running house:

   ```
   docker compose exec homeostat homeostat plan /house --bus tcp/127.0.0.1:7447
   ```

   `plan` validates everything (an unknown entity in a view, a parameter
   outside its constraint, a grant nobody declared) and prints the change
   with its tier:

   - **parameter-only**: defaults changed, no restart;
   - **behavioral**: unit code or manifests changed, units restart;
   - **structural**: units created or removed, grants changed.

3. Stop there. **The owner applies**, after reading the plan. Do not run
   `homeostat apply` unless the person you work for asked you to, in so
   many words, for this change.

A refused plan names stable error codes. `homeostat explain <code>` gives
the rule behind each one and why it exists; fix the cause rather than
working around the check.

## The contract

- `homeostat schema` (or `schema dashboard`, `schema unit`, …; run it
  like `plan`, through `docker compose exec`) prints the JSON Schema of
  every file above. It is generated from the parser, so it is complete.
  The same, rendered:
  <https://github.com/freol35241/homeostat/blob/main/docs/manifest.md>.
- The dashboard's widgets, with a picture of each:
  <https://github.com/freol35241/homeostat/blob/main/docs/widgets.md>.
  Each view in the dashboard has a **Text** button showing the
  `dashboard.toml` block behind it, so "make the second chart on the
  heating view a week long" can be resolved to one line.
- The design and its reasons:
  <https://github.com/freol35241/homeostat/blob/main/docs/design.md>.

## Looking at the running house

`units/mcp.toml` serves a read-only MCP surface on `:8642` (the README
says how to connect): `read_state`, `read_history`, `read_logs`,
`read_events`, `schema`, `explain`. Use it to see what the house is doing
before changing what it should do: which devices are unconfigured
(`home/discovery/{adapter}`), what a sensor has read this week, why a
unit is restarting.

## Rules of thumb

- No protocol reports which room a device is in. Ask rather than
  guessing.
- Prefer a parameter to new code. The family can tune a parameter from
  the dashboard, and that change needs no plan.
- Keep a change to what was asked. A plan that is larger than the
  request is harder to review, and the owner reviews every line.
- Never put a secret, a password hash or a token in the repo.
