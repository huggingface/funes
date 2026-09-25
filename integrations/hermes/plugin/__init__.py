"""funes automation for hermes: the lifecycle hooks that keep the memory current.

Each completed turn is converted out of hermes's SQLite store into the turns file funes indexes, in
this process — it is a read and an atomic write — and the index is spawned detached and forgotten,
as is the publish at a session boundary, so nothing here waits on an embedder or a network. The
publish takes its memory from the `memory` file beside this plugin, written by
`funes add hermes <memory>`; with no memory bound, the file is absent and the publish hooks are not
registered.
"""

import os
import subprocess
import time

from . import convert

HERE = os.path.dirname(os.path.abspath(__file__))


def _beside(name):
    """A path `funes add hermes` recorded beside this plugin, or None when it did not."""
    try:
        with open(os.path.join(HERE, name), encoding="utf-8") as f:
            return f.read().strip() or None
    except OSError:
        return None


def _convert(session_id):
    """The session that just spoke, as a turns file in the spool; returns its id. hermes names it
    when its hook carries one; otherwise it is the session whose message landed last, which is the
    same session. A hook must never raise into hermes, so a store this cannot read costs the turn's
    capture and nothing else."""
    spool, db = _beside("spool"), _beside("state-db")
    if not spool or not db:
        return None
    try:
        session_id = session_id or convert.latest_session_id(db)
        convert.convert(db, spool, session_id)
        return session_id
    except Exception:
        return None


def _convert_stale(named):
    """The sessions that spoke since the last sweep: those whose own hook never ran, a hermes that
    died mid-turn among them. What the spool holds cannot say which those are (funes drains it), so
    the sweep keeps its own mark: the previous sweep's stamp, or, before there is one, the `spool`
    record `funes add hermes` wrote just before it converted the history. The session that just
    spoke was converted already and is skipped."""
    spool, db = _beside("spool"), _beside("state-db")
    if not spool or not db:
        return
    mark = os.path.join(HERE, "swept")
    try:
        since = os.path.getmtime(mark if os.path.exists(mark) else os.path.join(HERE, "spool"))
        # Stamped before the sweep, so a session that speaks while it runs is swept again next turn.
        now = time.time()
        for session_id in convert.session_ids_since(db, since):
            if session_id != named:
                convert.convert(db, spool, session_id)
        with open(mark, "a", encoding="utf-8"):
            pass
        os.utime(mark, (now, now))
    except Exception:
        pass


def _spawn(script, *args):
    subprocess.Popen(
        ["bash", os.path.join(HERE, script), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


def _on_turn(**kwargs):
    _convert_stale(_convert(kwargs.get("session_id")))
    _spawn("funes-index.sh")


def register(ctx):
    ctx.register_hook("post_llm_call", _on_turn)
    if os.path.exists(os.path.join(HERE, "memory")):
        # `on_session_start` catches up whatever a session that never finalized left unpublished.
        for event in ("on_session_start", "on_session_finalize"):
            ctx.register_hook(event, lambda **kwargs: _spawn("funes-index.sh", "--publish"))
