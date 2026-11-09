// A Testcontainers test run from the job side through the services endpoint.
// Env: DOCKER_HOST, TESTCONTAINERS_HOST_OVERRIDE=localhost. Ryuk is on unless TESTCONTAINERS_RYUK_DISABLED=true.
const { IMAGE } = require("./tc-common");
const { GenericContainer, Wait } = require("testcontainers");
const { Client } = require("pg");


(async () => {
  const t0 = Date.now();
  const c = await new GenericContainer(IMAGE)
    .withEnvironment({ POSTGRES_PASSWORD: "tc" })
    .withExposedPorts(5432)
    .withWaitStrategy(Wait.forLogMessage(/ready to accept connections/, 2))
    .start();
  const host = c.getHost();
  const port = c.getMappedPort(5432);
  const pg = new Client({ host, port, user: "postgres", password: "tc" });
  await pg.connect();
  const r = await pg.query("select 41 + 1 as answer");
  await pg.end();
  await c.stop();
  const ok = r.rows[0].answer === 42;
  console.log(JSON.stringify({ ok, host, port, ms: Date.now() - t0 }));
  if (!ok) process.exit(1);
})().catch((e) => {
  console.error("FAIL", e.message);
  process.exit(1);
});
