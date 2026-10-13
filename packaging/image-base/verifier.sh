#!/bin/sh
# Python for verify.py, in the build stage only (runtime images ship none).
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get install -y -qq --no-install-recommends python3 >/dev/null
