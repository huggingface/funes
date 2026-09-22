"""funes automation for hermes: the lifecycle hooks that keep the memory current.

Each hook spawns one of the two scripts beside this file and returns — they detach a worker of
their own, so nothing here waits on an index or a publish. The publish script takes its memory from
the `memory` file beside it, written by `funes add hermes <memory>`; with no memory bound, the file
is absent and the publish hooks are not registered.
"""

import os
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = "hermes"


def _spawn(script, *args):
    subprocess.Popen(
        ["bash", os.path.join(HERE, script), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


def register(ctx):
    ctx.register_hook("post_llm_call", lambda **kwargs: _spawn("funes-index.sh", HARNESS))
    if os.path.exists(os.path.join(HERE, "memory")):
        # `on_session_start` catches up whatever a session that never finalized left unpublished.
        for event in ("on_session_start", "on_session_finalize"):
            ctx.register_hook(event, lambda **kwargs: _spawn("funes-push.sh", "", HARNESS))
