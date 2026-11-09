// Five Testcontainers containers started at once with Promise.all, each
// queried and stopped. Same environment as tc.test.js.
const { IMAGE } = require("./tc-common");
const { GenericContainer, Wait } = require("testcontainers");
const { Client } = require("pg");

const COUNT = 5;

async function one(n) {
  const c = await new GenericContainer(IMAGE)
    .withEnvironment({ POSTGRES_PASSWORD: "tc" })
    .withExposedPorts(5432)
    .withWaitStrategy(Wait.forLogMessage(/ready to accept connections/, 2))
    .start();
  const pg = new Client({ host: c.getHost(), port: c.getMappedPort(5432), user: "postgres", password: "tc" });
  await pg.connect();
  const r = await pg.query("select $1::int + 1 as answer", [n]);
  await pg.end();
  const port = c.getMappedPort(5432);
  await c.stop();
  return { n, port, ok: r.rows[0].answer === n + 1 };
}

(async () => {
  const t0 = Date.now();
  const results = await Promise.all(Array.from({ length: COUNT }, (_, n) => one(n)));
  const ports = new Set(results.map((r) => r.port));
  const ok = results.every((r) => r.ok) && ports.size === COUNT;
  console.log(JSON.stringify({ ok, count: COUNT, distinct_ports: ports.size, ms: Date.now() - t0 }));
  if (!ok) process.exit(1);
})().catch((e) => {
  console.error("FAIL", e.message);
  process.exit(1);
});
