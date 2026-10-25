# gog (github.com/steipete/gogcli) as a marsh Kit, behind a safety wrapper.
# CI passes the latest release and its checksum; these defaults are only for
# local builds.
FROM docker.io/mcavage/marsh-shell:latest AS fetch
ARG GOG_VERSION=0.46.0
ARG GOG_SHA256=9ae69d117cef895aa1c50b5859d7d3def45c5580886b25dc1c7f3cfe128ed0dd
ADD --chmod=0644 --checksum=sha256:${GOG_SHA256} \
  https://github.com/steipete/gogcli/releases/download/v${GOG_VERSION}/gogcli_${GOG_VERSION}_linux_arm64.tar.gz /tmp/gog.tar.gz
RUN mkdir /tmp/gog && tar -xzf /tmp/gog.tar.gz -C /tmp/gog

FROM docker.io/mcavage/marsh-shell:latest
COPY --from=fetch --chmod=0755 /tmp/gog/gog /usr/local/libexec/gog
COPY --chmod=0644 allowed-commands /usr/local/share/gog-allowed-commands
COPY --chmod=0755 gog /usr/local/bin/gog
ENTRYPOINT ["/usr/local/bin/gog"]
