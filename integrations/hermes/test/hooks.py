"""The plugin's hooks, driven as hermes drives them: a turn converts the session that spoke and
spawns the index script; a session boundary spawns it in publish mode — both through `sh`, the one
shell a box running hermes is promised. Loads the plugin as installed, with a fake index script
that records what it was launched for.

    python3 integrations/hermes/test/hooks.py    (run.sh runs it)
"""

import importlib.util
import os
import shutil
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.join(HERE, "..", "plugin")


def fail(why):
    sys.exit(f"hermes hooks: {why}")


tmp = tempfile.mkdtemp(prefix="funes-hermes-hooks-")
# Only `sh` is promised on a box running hermes: a `bash` that fails proves nothing is spawned
# through it.
shims = os.path.join(tmp, "shims")
os.makedirs(shims)
with open(os.path.join(shims, "bash"), "w", encoding="utf-8") as f:
    f.write("#!/bin/sh\nexit 127\n")
os.chmod(os.path.join(shims, "bash"), 0o755)
os.environ["PATH"] = shims + os.pathsep + os.environ.get("PATH", "")

# The plugin as installed: its files, the records `setup add` writes beside them, and an index
# script that records what it was launched for.
dest = os.path.join(tmp, "funes")
shutil.copytree(PLUGIN, dest, ignore=shutil.ignore_patterns("test", "__pycache__"))
spool = os.path.join(tmp, "spool")
os.makedirs(spool)
db = os.path.join(tmp, "state.db")
shutil.copy(os.path.join(PLUGIN, "test", "state.db"), db)
for name, value in (("spool", spool), ("state-db", db), ("memory", "acme/kb")):
    with open(os.path.join(dest, name), "w", encoding="utf-8") as f:
        f.write(value + "\n")
log = os.path.join(tmp, "worker.log")
script = os.path.join(dest, "funes-index.sh")
with open(script, "w", encoding="utf-8") as f:
    f.write(f"#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"{log}\"\n")
os.chmod(script, 0o755)

# Loaded as the package hermes loads.
spec = importlib.util.spec_from_file_location("funes", os.path.join(dest, "__init__.py"), submodule_search_locations=[dest])
plugin = importlib.util.module_from_spec(spec)
sys.modules["funes"] = plugin
spec.loader.exec_module(plugin)

hooks = {}


class Ctx:
    def register_hook(self, event, fn):
        hooks.setdefault(event, []).append(fn)


plugin.register(Ctx())
if set(hooks) != {"post_llm_call", "on_session_start", "on_session_finalize"}:
    fail(f"registered {sorted(hooks)}")

# A turn of the fixture's session: converted into the spool, then the index spawned.
for fn in hooks["post_llm_call"]:
    fn(session_id="20260101_000000_fixture")
if not os.path.isfile(os.path.join(spool, "20260101_000000_fixture.funes.jsonl")):
    fail("the turn was not converted")
# A boundary: the publish.
for fn in hooks["on_session_finalize"]:
    fn()

deadline = time.time() + 10
expected = "\n--publish\n"
while True:
    try:
        with open(log, encoding="utf-8") as f:
            got = f.read()
    except OSError:
        got = ""
    if got == expected:
        break
    if time.time() > deadline:
        fail(f"the index script was launched with {got!r}, expected {expected!r}")
    time.sleep(0.05)
shutil.rmtree(tmp)
print("hermes hooks: ok")
