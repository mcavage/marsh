// Self-hosted Renovate settings that cannot live in the repository's own
// configuration (credentials, allowed commands). The repository configuration is
// .github/renovate.json5. Used by .github/workflows/renovate.yml.
const hostRules = [];
if (process.env.DOCKER_HUB_USERNAME && process.env.DOCKER_HUB_TOKEN) {
  // One Docker account reads both docker.io and the Docker Hardened Images
  // registry (dhi.io). A read-only token is enough.
  for (const matchHost of ['docker.io', 'dhi.io']) {
    hostRules.push({
      matchHost,
      hostType: 'docker',
      username: process.env.DOCKER_HUB_USERNAME,
      password: process.env.DOCKER_HUB_TOKEN,
    });
  }
}

module.exports = {
  platform: 'github',
  repositories: ['mcavage/marsh'],
  onboarding: false,
  requireConfig: 'required',
  hostRules,
  // The only command a bump may run: regenerate a Kit's release-checksums.txt
  // from the publisher's metadata (scripts/refresh-agent-checksums.py).
  allowedCommands: ['^python3 scripts/refresh-agent-checksums\\.py$'],
};
