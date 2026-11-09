// The integration test of J3. It starts a Postgres through Testcontainers (Ryuk
// on by default). When NFX_HOLD is set to a path, it performs the explicit
// handshake the journey relies on: it writes `<path>.up` once the container is
// running and its data written, then waits (up to 20 min) for `<path>.release`
// before asserting, so a checkpoint can be taken while the container exists.
const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const { GenericContainer, Wait } = require("testcontainers");
const repo = require("../src/repo");

test("users repository against a fresh Postgres", async () => {
  const container = await new GenericContainer("postgres:17@sha256:748a18a54d3842d43bc156f0840d78029b8d349e0feca98c12b9c4ce3404a6b3")
    .withEnvironment({ POSTGRES_PASSWORD: "tc", POSTGRES_DB: "app" })
    .withExposedPorts(5432)
    .withWaitStrategy(Wait.forLogMessage(/ready to accept connections/, 2))
    .start();
  try {
    const url = `postgresql://postgres:tc@${container.getHost()}:${container.getMappedPort(5432)}/app`;
    await repo.migrate(url);
    await repo.addUser(url, "ada");
    if (process.env.NFX_HOLD) {
      fs.writeFileSync(`${process.env.NFX_HOLD}.up`, container.getId());
      const end = Date.now() + 20 * 60 * 1000;
      while (!fs.existsSync(`${process.env.NFX_HOLD}.release`) && Date.now() < end) await new Promise((r) => setTimeout(r, 500));
    }
    assert.strictEqual(await repo.countUsers(url), 1);
  } finally {
    await container.stop();
  }
});
