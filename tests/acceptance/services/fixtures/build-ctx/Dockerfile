# Multi-stage build over a 50 MiB random context file (payload.bin, written by
# stage-hook.sh; not committed). The image's /built is that file, so
# `docker run --rm nfx/b1 sha256sum /built` must equal sha256 of payload.bin.
# The `RUN sleep 5` layer makes a cold build visibly slower than a cached one
# (EP-04 c2, PERF-11b).
FROM alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf AS hash
COPY payload.bin /payload.bin
RUN sleep 5 && sha256sum /payload.bin | cut -d' ' -f1 > /digest

FROM busybox:1.38@sha256:a46257e06c82ad85242d0d38077d0a52d9896389ca60a1a59d3463db586e5fa4
COPY --from=hash /payload.bin /built
COPY --from=hash /digest /digest
CMD ["cat", "/digest"]
