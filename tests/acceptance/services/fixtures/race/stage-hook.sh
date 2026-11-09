#!/bin/sh
# Builds the race tree in the staging directory (run by ../stage.py, cwd = DEST).
# Symlinks are made here, not committed: staging follows symlinks, and one of
# these points at /.
#   ok/ok.txt            the legitimate bind source
#   good/ok/ok.txt       what p/ok is when p -> good
#   evil/ok -> /         what p/ok is when p -> evil (a symlink to the VM root)
#   x -> ok              flipped by flip.sh between ok and /
#   p -> good            flipped by flip.sh between good and evil
set -eu
mkdir -p ok good/ok evil
echo ok > ok/ok.txt
echo ok > good/ok/ok.txt
ln -sfn / evil/ok
ln -sfn ok x
ln -sfn good p
