#!/bin/sh
# Run after apt-get update. DHI bases can contain versions that sort above
# the matching packages in the apt repository. Align the small set needed by
# our build tools, allowing downgrades only for these explicit versions.
set -eu
exact_dependency() {
  apt-cache show --no-all-versions "$1" |
    sed -n "s/^Depends:.*$2 (= \([^)]*\)).*/\1/p"
}
set --
for pair in perl:perl-base gcc-14:gcc-14-base libacl1-dev:libacl1 libgmp-dev:libgmp10; do
  parent=${pair%:*} package=${pair#*:}
  version=$(exact_dependency "$parent" "$package")
  test -n "$version"
  set -- "$@" "$package=$version"
  if [ "$package" = gcc-14-base ]; then
    # GCC runtime libraries depend on exactly the same base revision. Move
    # installed members of that source package together in one transaction.
    installed=$(dpkg-query -W -f '${Package} ${source:Package} ${db:Status-Abbrev}\n' |
      awk '$2 == "gcc-14" && $3 == "ii" { print $1 }')
    for runtime in $installed; do
      set -- "$@" "$runtime=$version"
    done
  fi
done
apt-get install -y --no-install-recommends --allow-downgrades "$@"
