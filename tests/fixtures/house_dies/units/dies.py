# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../../sdk/python", editable = true }
# ///
"""A unit that fails at startup after declaring a subscriber.

With `raise` it raises from `main()`; with `exit` it calls `sys.exit()`.
zenoh runs a subscriber's callback on a thread the interpreter waits for
at exit, so either way the process exits only if the session is closed.
The session is a module global, so the interpreter does not free it on
the way out.
"""

import sys

from homeostat import session

SESSION = session.connect()


def main():
    SESSION.subscribe("home/clock/*", lambda _sample: None)
    if sys.argv[1] == "exit":
        sys.exit("startup failed")
    raise RuntimeError("startup failed")


if __name__ == "__main__":
    main()
