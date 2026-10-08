FROM dhi.io/sbx-templates:claude-code-docker@sha256:492664da2ac39b2796802b2dbe2472b16e7e5c21781744f457af6d79f2a85d80
COPY --chmod=0755 agent.mjs /usr/local/bin/marsh-acp-fixture.mjs
USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["node", "/usr/local/bin/marsh-acp-fixture.mjs"]
