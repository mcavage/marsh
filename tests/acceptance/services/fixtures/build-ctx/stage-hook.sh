#!/bin/sh
# 50 MiB of random bytes: incompressible, so the context upload cost is real.
set -eu
head -c 52428800 /dev/urandom > payload.bin
