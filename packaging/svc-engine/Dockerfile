# syntax=docker/dockerfile:1
# marsh-svc-engine: the container that owns a project's production Docker
# engine inside its services VM (SECURITY-DESIGN A1). It holds dockerd and
# containerd at the shell image's pinned versions, the XFS and mount tools the
# pool needs, and nothing else. Nothing is installed or fetched when it runs:
# `marsh --internal-services engine-up` is bind-mounted from the VM at
# /marsh/marsh and is the entrypoint. The caller supplies the reviewed DHI
# base digest (packaging/shell-image) and records the built image by digest.
# The Compose plugin is in it because restore (S4) runs the compose client here,
# as an unprivileged user on the agent's filtered socket (SECURITY-DESIGN A2);
# it is pinned like dockerd and the build fails without it, or when the binary
# does not report the pinned package's upstream version (`X.Y.Z-R` -> vX.Y.Z;
# a `~rc` or `+dfsg` package version does not map and fails the build).
# That pin is the one Compose version; tests/release/test_compose_pins.py keeps
# every other copy equal to it.
# iptables and ip6tables are in it for the egress firewall `engine-up` installs
# before the production engine starts (SECURITY-DESIGN A12); they are declared
# here, and the build fails without them, rather than left to a dependency of
# docker-ce.
ARG SVC_ENGINE_BASE
FROM ${SVC_ENGINE_BASE}
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      docker-ce=5:29.9.3-2 docker-ce-cli=5:29.9.3-2 containerd.io=2.4.2-7 \
      docker-compose-plugin=5.7.1-1 \
      xfsprogs util-linux mount iproute2 procps iptables \
 && apt-get clean \
 && rm -rf /var/lib/apt/lists/* \
 && command -v dockerd && command -v containerd && command -v mkfs.xfs \
 && command -v mount && command -v fallocate && command -v mknod && command -v findmnt \
 && command -v iptables && command -v ip6tables \
 && test -x /usr/libexec/docker/cli-plugins/docker-compose \
 && compose=$(dpkg-query -W -f='${Version}' docker-compose-plugin) && test -n "$compose" \
 && compose=${compose#*:} && reported=$(docker compose version --short) \
 && test -n "${reported#v}" && test "${reported#v}" = "${compose%-*}" \
 && dockerd --version
LABEL dev.marsh.image="svc-engine"
ENTRYPOINT ["/marsh/marsh", "--internal-services", "engine-up"]
