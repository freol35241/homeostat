# Test fixtures

The `house*` directories are houses the integration tests run.

Three files here each hold a rule that more than one implementation
checks. Each implementation's tests read the same file:

| File | The rule | Read by |
|---|---|---|
| `host_gate.json` | the Host values the dashboard and the MCP server admit | `src/mcp/http.rs` and `tests/it/dashboard.rs`, against the dashboard |
| `key_segments.json` | a name usable as one bus key segment | `src/validate.rs` and `sdk/python/tests/test_keys.py` |
| `time_of_day.json` | the `"HH:MM"` time of day | `src/validate.rs`, `sdk/python/tests/test_keys.py`, `tests/adapters/test_arbiter.py` and `tests/js/dashboard-logic.test.js` |

Do not edit one of these files to make one implementation pass. When
they disagree on a case, one of them has a bug, and the fix goes there.
