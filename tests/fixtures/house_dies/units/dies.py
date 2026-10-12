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

zenoh runs a subscriber's callback on a thread the interpreter waits for
at exit, so the process exits only if the session is closed.
"""

from homeostat import session


def main():
    sess = session.connect()
    sess.subscribe("home/clock/*", lambda _sample: None)
    raise RuntimeError("startup failed")


if __name__ == "__main__":
    main()
