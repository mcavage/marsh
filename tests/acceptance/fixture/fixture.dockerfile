ARG SHELL_IMAGE
FROM rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build

WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM ${SHELL_IMAGE}

USER root
COPY image-repair /tmp/marsh-image-repair
RUN python3 -I -S /tmp/marsh-image-repair/repair.py \
 && rm -rf /tmp/marsh-image-repair
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --receipt

COPY --from=build /source/target/release/marsh-acceptance-fixture /usr/local/bin/marsh-fixture
# The image ships its own `fixture` on PATH (processes acceptance: the entry
# case execs it; a job of another registered name keeps it local), and one
# image-config variable that must never be forwarded to a spawned child.
RUN ln -s marsh-fixture /usr/local/bin/fixture
ENV FIXTURE_IMAGE_ENV=image-config

USER agent
ENTRYPOINT ["/usr/local/bin/marsh-fixture"]
