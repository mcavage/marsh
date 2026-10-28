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
  // Renovate runs no command: CI calls renovate-refresh.yml to prepare
  // records before validating each renovate/** pull request.
};
