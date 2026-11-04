# Work tools for agents as one marsh Kit: gog (github.com/steipete/gogcli)
# behind a safety wrapper, and the BambooHR directory tool. gog's version and
# checksum are pinned here, and CI builds exactly these: a gog upgrade is a
# deliberate change to these two lines, with its new commands and flags
# classified in gog/ (see the Kit's README).
FROM docker.io/mcavage/marsh-shell:latest AS fetch
ARG GOG_VERSION=0.47.1
ARG GOG_SHA256=c677ac6abf5e187e987a87fe1b588f454b002a7d7019de81b9e8db02e9e902b6
ADD --chmod=0644 --checksum=sha256:${GOG_SHA256} \
  https://github.com/steipete/gogcli/releases/download/v${GOG_VERSION}/gogcli_${GOG_VERSION}_linux_arm64.tar.gz /tmp/gog.tar.gz
RUN mkdir /tmp/gog && tar -xzf /tmp/gog.tar.gz -C /tmp/gog

FROM docker.io/library/golang:1 AS bamboohr
WORKDIR /src
COPY bamboohr/go.mod bamboohr/*.go ./
RUN go test ./... && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -ldflags=-s -o /out/bamboohr .

FROM docker.io/mcavage/marsh-shell:latest
COPY --from=fetch --chmod=0755 /tmp/gog/gog /usr/local/libexec/gog
COPY --chmod=0644 gog/allowed-commands /usr/local/share/gog-allowed-commands
COPY --chmod=0644 gog/flag-rules /usr/local/share/gog-flag-rules
COPY --chmod=0755 gog/gog /usr/local/libexec/marsh-tools/gog
COPY --from=bamboohr --chmod=0755 /out/bamboohr /usr/local/libexec/marsh-tools/bamboohr
COPY --chmod=0755 marsh-tools /usr/local/bin/marsh-tools
ENTRYPOINT ["/usr/local/bin/marsh-tools"]
