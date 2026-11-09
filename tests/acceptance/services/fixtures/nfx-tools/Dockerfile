# nfx-tools (SEC-15): the container a job starts to test substituted Docker
# sockets at depth 1 and 2. The docker CLI (+ compose, buildx) and python3 on a
# small userland, with rawdocker.py and its cases baked in at /nfx. Built from
# the pinned docker:cli image with exact apk versions. The build is reproducible
# (timestamps rewritten, apk's log removed), so its image ID is pinned in
# local-images.json and checked on every save and load; a changed input fails
# loudly instead of producing a different image under the same tag.
FROM docker:29.9.2-cli@sha256:5e6baf67325a79f64ed3bed6863954d5e05d0f3d4e57cbdb14809346814bbfdb
RUN apk add --no-cache python3=3.14.10-r1 curl=8.23.0-r0 jq=1.8.2-r0 && rm -f /var/log/apk.log
COPY nfx /nfx
ENV PATH="/nfx:${PATH}"
WORKDIR /nfx
ENTRYPOINT []
CMD ["sh"]
