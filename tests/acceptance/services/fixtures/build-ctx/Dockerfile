# Multi-stage build over a 50 MiB random context file (payload.bin, written by
# stage-hook.sh; not committed). The image's /built is that file, so
# `docker run --rm nfx/b1 sha256sum /built` must equal sha256 of payload.bin.
# The `RUN sleep 5` layer makes a cold build visibly slower than a cached one
# (EP-04 c2, PERF-11b).
FROM alpine:3.22@sha256:bfe7ceb9f09ec0566c48fb93356e45a47a00be5a1dd14fa5b909e7709c66b034 AS hash
COPY payload.bin /payload.bin
RUN sleep 5 && sha256sum /payload.bin | cut -d' ' -f1 > /digest

FROM busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec
COPY --from=hash /payload.bin /built
COPY --from=hash /digest /digest
CMD ["cat", "/digest"]
