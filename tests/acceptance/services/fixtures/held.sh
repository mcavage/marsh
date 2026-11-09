#!/bin/sh
# A generic held job: prints READY, then blocks until the harness sends one line
# on stdin. `held.sh CMD...` runs CMD after the release.
echo READY
read -r _release
[ "$#" -gt 0 ] && exec "$@"
exit 0
