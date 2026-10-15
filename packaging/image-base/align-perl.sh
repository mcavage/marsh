#!/bin/sh
# Run after apt-get update. DHI can ship a newer perl-base without publishing
# the matching full Perl package. Debian's Perl (needed by git/autoconf) has an
# exact-version dependency on perl-base. Align only that package, rather than
# allowing downgrades across the subsequent tool installation. The resulting
# Debian package and its copyright are included in the normal image inventory.
set -eu
version=$(apt-cache show --no-all-versions perl |
  sed -n 's/^Depends:.*perl-base (= \([^)]*\)).*/\1/p')
test -n "$version"
apt-get install -y --no-install-recommends --allow-downgrades "perl-base=$version"
