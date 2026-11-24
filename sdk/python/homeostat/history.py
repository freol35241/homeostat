"""The recorder's history reads: selectors, and reading their replies.

The recorder answers reads on home/history/** (adapters/recorder.py,
docs/design.md#history-and-the-recorder). A read is a key with
parameters in zenoh's selector form, ``key?name=value;name=value``.
`selector` writes that form and `split_params` reads it back, so the SDK,
the dashboard and the recorder share one grammar. Times on the samples
and forecast paths are RFC3339 with an offset. The events path takes
integer microseconds.
"""

import datetime
import time
from collections.abc import Mapping

from .session import QueryError, QueryTimeout, UnitSession

# The most rows (issues, on the forecast path) the recorder returns for
# one read. It clamps a larger limit to this and keeps the newest, so a
# reader that gets this many cannot tell whether the window held more.
MAX_ROWS = 10_000

# The recorder's store description. It is the only history read that
# answers whenever the recorder is up, whatever the store holds.
STATS_KEY = "home/history/stats"


class HistoryUnavailable(Exception):
    """`Context.history`, `Context.forecast_history` or `read` could not read.

    The house runs no recorder, the recorder did not answer or answered
    with an error, or the read holds more rows or issues than one read
    returns.
    """


def selector(key: str, params: Mapping[str, str | int | datetime.datetime]) -> str:
    """Return the selector that reads `key` with `params`, in the mapping's order.

    A datetime is written as RFC3339 in UTC. A naive one is read as local
    time. Other values are written with `str`. Nothing is URL-encoded,
    because an RFC3339 offset's '+' must stay literal and the recorder
    does not decode.

    Parameters
    ----------
    key : str
        The key expression to read, such as a `keys.history_key`.
    params : Mapping of str to str, int or datetime.datetime
        The parameters, by name.

    Returns
    -------
    str
        ``key?name=value;name=value``.
    """
    parts = []
    for name, value in params.items():
        if isinstance(value, datetime.datetime):
            value = value.astimezone(datetime.timezone.utc).isoformat()
        parts.append(f"{name}={value}")
    return key + "?" + ";".join(parts)


def split_params(raw: str) -> dict[str, str]:
    """Split a selector's parameters into a dict, as the recorder reads them.

    Nothing is URL-decoded (see `selector`). Empty parts are skipped.

    Parameters
    ----------
    raw : str
        The parameters after the selector's '?'.

    Returns
    -------
    dict of str to str
        Each parameter's value, by name. A name without '=' maps to ''.
    """
    params = {}
    for part in raw.split(";"):
        if not part:
            continue
        name, _, value = part.partition("=")
        params[name] = value
    return params


def wait(session: UnitSession, timeout_s: float) -> str | None:
    """Wait for the recorder to answer, and return why it did not, or None.

    This asks `stats`, not the series to be read. The recorder replies
    once per series it holds, so a series it does not hold gets no reply
    at all, and waiting on it would take the whole timeout. `stats`
    describes the store and always answers while the recorder is up.

    Send one query at a time, and let it wait for the rest of the
    deadline. The recorder answers queries one at a time (zenoh runs a
    queryable's callback serially). A get that gives up and asks again
    leaves its query in the recorder's queue. Once an answer takes
    longer than the get's timeout, every later answer arrives after its
    get has given up, and the loop never receives one. A get that no
    queryable serves returns at once with no replies, which means the
    recorder is not up yet. Only that case is retried.

    Parameters
    ----------
    session : UnitSession
        The session to ask on.
    timeout_s : float
        Seconds to keep waiting.

    Returns
    -------
    str or None
        Why the recorder did not answer, or None once it has.
    """
    deadline = time.monotonic() + timeout_s
    while True:
        remaining = max(deadline - time.monotonic(), 0.1)
        try:
            if session.get_json(STATS_KEY, timeout_s=remaining):
                return None
        except QueryTimeout:
            return f"recorder took the query but did not answer within {timeout_s}s"
        except QueryError as err:
            return str(err)
        if time.monotonic() >= deadline:
            return f"no recorder answered within {timeout_s}s"
        time.sleep(0.2)


def read(session: UnitSession, query: str, timeout_s: float) -> list:
    """Wait for the recorder, then return the first series' reply to `query`.

    The recorder sends nothing for a series it has never recorded, and a
    get nobody serves returns nothing too. On an empty reply this asks
    `stats` again to tell the two apart, so a recorder that went down
    since the wait does not read as an empty window.

    Parameters
    ----------
    session : UnitSession
        The session to ask on.
    query : str
        A `selector`.
    timeout_s : float
        Seconds to wait for the recorder, and then for each reply.

    Returns
    -------
    list
        The rows or issues of the first series that replied, or an empty
        list when none did.

    Raises
    ------
    HistoryUnavailable
        If the recorder does not answer, answers with an error, or stops
        answering after the wait.
    """
    reason = wait(session, timeout_s)
    if reason is not None:
        raise HistoryUnavailable(reason)
    try:
        replies = session.get_json(query, timeout_s=timeout_s)
        if not replies and not session.get_json(STATS_KEY, timeout_s=timeout_s):
            raise HistoryUnavailable("the recorder stopped answering")
    except QueryError as err:
        raise HistoryUnavailable(str(err)) from err
    return next((payload for _, payload in replies), [])
