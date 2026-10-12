FROM dhi.io/sbx-templates:claude-code-docker@sha256:e3b7baee8a6f5f3805201b52b27c192e03603edbe1bea2d17d9399717777fd29
COPY --chmod=0755 agent.mjs /usr/local/bin/marsh-acp-fixture.mjs
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["node", "/usr/local/bin/marsh-acp-fixture.mjs"]
