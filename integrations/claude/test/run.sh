#!/bin/sh
# Every test of the claude bundle: the converter, then `setup` and the hook worker, each run as
# funes runs them, against fakes.
#
#   sh integrations/claude/test/run.sh
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
sh "$HERE/../claude-plugin/funes/test/run.sh"
sh "$HERE/setup.sh"
sh "$HERE/hooks.sh"
