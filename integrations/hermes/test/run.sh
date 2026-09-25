#!/bin/sh
# Every test of the hermes bundle: the converter, the plugin's hooks, then `setup`, run as funes
# runs it, against a fake.
#
#   sh integrations/hermes/test/run.sh
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
sh "$HERE/../plugin/test/run.sh"
python3 "$HERE/hooks.py"
sh "$HERE/setup.sh"
