# Hermes Agent (Nous Research) as a marsh Kit: a self-contained Python and
# Hermes, built in a hardened stage, on marsh's shell base.
FROM dhi.io/debian-base:trixie-dev AS build
ARG UV_VERSION=0.13.5
ARG HERMES_VERSION=0.22.5
ARG HERMES_COMMIT=bf0203a794a07aac3ff764972ab9087baa4e4f4e
ADD https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-aarch64-unknown-linux-gnu.tar.gz /tmp/uv.tar.gz
RUN tar -xzf /tmp/uv.tar.gz -C /tmp
ADD https://github.com/NousResearch/hermes-agent.git#${HERMES_COMMIT} /opt/hermes/src
# Hermes installs from its lockfile, editable (it refuses wheel builds), into a
# Python that lives under /opt/hermes, so the result is copied as one tree.
ENV UV_PYTHON_INSTALL_DIR=/opt/hermes/python UV_LINK_MODE=copy UV_NO_CACHE=1
RUN cd /opt/hermes/src \
 && /tmp/uv-aarch64-unknown-linux-gnu/uv sync --frozen --no-dev --extra anthropic --extra mcp \
      --python 3.14 --python-preference only-managed
# The checkout has no git history, so Hermes cannot tell its own version:
# `hermes --version` would print "vunknown" and the agent guesses from the
# stale release date in the source. The install stamp is upstream's way for a
# packager to say which release this is, and that updates are external.
RUN cd /opt/hermes/src \
 && .venv/bin/python scripts/write_install_stamp.py --output install-stamp.json \
      --commit "$HERMES_COMMIT" --base-version "$HERMES_VERSION" --distance 0 \
      --source ci --distribution docker --update-mechanism external

FROM docker.io/mcavage/marsh-shell@sha256:030b8c4f5a2f92af66f314ee48f4569c4b29ac3adbef834f481d51c1210c5499
COPY --from=build /opt/hermes /opt/hermes
COPY defaults /opt/hermes/defaults
COPY --chmod=0755 entrypoint.sh /usr/local/bin/hermes
ENTRYPOINT ["/usr/local/bin/hermes"]
