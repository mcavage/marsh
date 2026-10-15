#!/bin/sh
# Run the Kit VM floor's commands, and any named extra commands, in a
# throwaway stage built from the image, so a missing library or loader fails
# the build rather than the first Kit VM boot.
#
#   smoke.sh [COMMAND...]
set -eu
for command in dockerd containerd runc docker git curl bash tar "$@"; do
  "$command" --version >/dev/null
done
# Git's Perl tools need the full interpreter, not just the base package.
perl -e 'use strict; use warnings; use File::Spec;'
test -x /bin/sh && test -x /bin/bash
test -d /usr/lib/locale/C.utf8
# Docker Sandboxes installs its proxy CA this way at VM start.
test -d /usr/local/share/ca-certificates
update-ca-certificates >/dev/null
identity=$(id agent)
case "$identity" in
  "uid=1000(agent) gid=1000(agent) groups=1000(agent),999(docker)") ;;
  *) echo "smoke: unexpected sandbox account: $identity" >&2; exit 1 ;;
esac
touch /smoke.ok
