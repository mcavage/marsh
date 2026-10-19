#!/bin/sh
# Assemble Debian packages onto a DHI runtime base, which ships no package
# manager. Runs as root in a debian-base -dev build stage of the same Debian
# release:
#
#   assemble-packages.sh BASE ROOTFS PACKAGE...
#
# BASE holds the runtime base's /var/lib/dpkg and /etc/{passwd,group} (copied
# from it with COPY --from). The named packages are installed here with apt
# (no recommends), and every package of their dependency closure that the base
# lacks, or has at another version, is copied into ROOTFS with its file list,
# md5sums, copyright file and a status entry in the base's own format
# (status.d, plus the status file when the base has one). ROOTFS also gets the
# base's account files with the sandbox `agent` user (uid 1000, home
# /home/agent) and its `docker` group. The final stage copies ROOTFS over the
# runtime base.
set -eu
base=$1 rootfs=$2
shift 2
# An argument @NAME expands to the package specs listed in NAME.packages next
# to this script (one per line; `#` comments).
here=$(cd "$(dirname "$0")" && pwd)
specs=
for spec in "$@"; do
  case "$spec" in
    @*) specs="$specs $(sed 's/#.*//' "$here/${spec#@}.packages" | tr '\n' ' ')" ;;
    *) specs="$specs $spec" ;;
  esac
done
# shellcheck disable=SC2086
set -- $specs
test "$#" -gt 0
{ test -d "$base/dpkg/status.d" || test -f "$base/dpkg/status"; } && test -f "$base/etc/passwd" && test -f "$base/etc/group"
for link in bin sbin lib; do
  test "$(readlink "/$link")" = "usr/$link"
done
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
sh "$here/align-perl.sh"
apt-get install -y -qq --no-install-recommends "$@" >/dev/null
names=
for spec in "$@"; do names="$names ${spec%%=*}"; done

base_version() {
  if [ -f "$base/dpkg/status.d/$1" ]; then
    sed -n 's/^Version: //p' "$base/dpkg/status.d/$1" | head -n 1
  elif [ -f "$base/dpkg/status" ]; then
    awk -v name="$1" '/^Package: / { found = ($2 == name) } found && /^Version: / { print $2; exit }' "$base/dpkg/status"
  fi
}

mkdir -p "$rootfs/var/lib/dpkg/info" "$rootfs/etc"
if [ -d "$base/dpkg/status.d" ]; then mkdir -p "$rootfs/var/lib/dpkg/status.d"; fi
selected=/tmp/marsh-assembled-packages
paths=/tmp/marsh-assembled-paths
: >"$selected"
: >"$paths"
apt-cache depends --recurse --installed --no-recommends --no-suggests --no-conflicts \
    --no-breaks --no-replaces --no-enhances $names | grep -v '^[ <]' | sort -u |
  while read -r package; do
    version=$(dpkg-query -W -f '${db:Status-Abbrev}${Version}' "$package" 2>/dev/null) || continue
    case "$version" in ii\ *) version=${version#ii } ;; *) continue ;; esac
    version=$(printf '%s' "$version" | sed 's/^ *//')
    test "$version" != "$(base_version "$package")" || continue
    printf '%s\n' "$package" >>"$selected"
  done

while read -r package; do
  dpkg-query -L "$package" | while read -r path; do
    case "$path" in
      /.) continue ;;
      /bin|/sbin|/lib) path=/usr$path ;;
      /bin/*|/sbin/*|/lib/*) path=/usr$path ;;
    esac
    if [ -e "$path" ] || [ -L "$path" ]; then printf '%s\n' "${path#/}" >>"$paths"; fi
  done
  for suffix in list md5sums; do
    for file in "/var/lib/dpkg/info/$package.$suffix" "/var/lib/dpkg/info/$package:$(dpkg --print-architecture).$suffix"; do
      if [ -f "$file" ]; then cp "$file" "$rootfs/var/lib/dpkg/info/$package.$suffix"; fi
    done
  done
  if [ -d "$base/dpkg/status.d" ]; then
    dpkg-query -s "$package" | sed '/^Status: /d' >"$rootfs/var/lib/dpkg/status.d/$package"
  fi
done <"$selected"

# update-alternatives links (iptables, awk, ...) are made by maintainer scripts,
# not listed by any package: keep each one whose selected target is assembled.
for alternative in /etc/alternatives/*; do
  test -L "$alternative" || continue
  target=$(readlink "$alternative")
  grep -qx "${target#/}" "$paths" || continue
  printf '%s\n' "${alternative#/}" >>"$paths"
  for directory in /usr/bin /usr/sbin; do
    for link in "$directory"/*; do
      if [ -L "$link" ] && [ "$(readlink "$link")" = "$alternative" ]; then
        printf '%s\n' "${link#/}" >>"$paths"
      fi
    done
  done
done

sort -u "$paths" | tar -C / --no-recursion -cf - -T - | tar -C "$rootfs" -xpf -
# DHI bases link /usr/bin/sh to bash outside any package; keep that shape
# when bash itself is assembled here.
if grep -qx bash "$selected"; then ln -sfn bash "$rootfs/usr/bin/sh"; fi
# They also link /usr/bin/which to which.gnu outside gnu-which, which ships
# only which.gnu.
if grep -qx gnu-which "$selected"; then ln -sfn which.gnu "$rootfs/usr/bin/which"; fi

if [ -f "$base/dpkg/status" ]; then
  awk -v keep="$selected" '
    BEGIN { while ((getline name < keep) > 0) replaced[name] = 1 }
    /^Package: / { skip = ($2 in replaced) }
    !skip { print }
  ' "$base/dpkg/status" >"$rootfs/var/lib/dpkg/status"
  while read -r package; do
    printf '\n' >>"$rootfs/var/lib/dpkg/status"
    dpkg-query -s "$package" >>"$rootfs/var/lib/dpkg/status"
  done <"$selected"
fi

# Accounts: root (the Kit VM's worker and Docker Engine run as root; DHI
# runtime bases list none) and the sandbox user, uid 1000 `agent` whatever the
# base called it, in the `docker` group.
{
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  awk -F: '$3 != 0 && $3 != 1000' "$base/etc/passwd"
  printf 'agent:x:1000:1000:agent:/home/agent:/bin/bash\n'
} >"$rootfs/etc/passwd"
{
  printf 'root:x:0:\n'
  awk -F: '$3 != 0 && $3 != 1000 && $3 != 999 && $1 != "docker"' "$base/etc/group"
  printf 'agent:x:1000:\ndocker:x:999:agent\n'
} >"$rootfs/etc/group"
install -d -m 0755 -o 1000 -g 1000 "$rootfs/home/agent" "$rootfs/home/agent/workspace"
# Docker Sandboxes adds its proxy CA through update-ca-certificates at VM
# start; ca-certificates' maintainer script, which DHI runtime bases never
# ran, creates the local certificate directory it reads.
install -d -m 0755 "$rootfs/usr/local/share/ca-certificates"
rm -f "$selected" "$paths"
