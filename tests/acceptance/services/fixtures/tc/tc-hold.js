// Starts one Testcontainers container and holds it for HOLD seconds (default
// 60), then stops it. Prints one JSON line when the container is up so a
// harness can wait for it. Same environment as tc.test.js.
const { IMAGE } = require("./tc-common");
const { GenericContainer, Wait } = require("testcontainers");

const HOLD = Number(process.env.HOLD || 60);

(async () => {
  const c = await new GenericContainer(IMAGE)
    .withEnvironment({ POSTGRES_PASSWORD: "tc" })
    .withExposedPorts(5432)
    .withWaitStrategy(Wait.forLogMessage(/ready to accept connections/, 2))
    .start();
  console.log(JSON.stringify({ up: true, id: c.getId(), port: c.getMappedPort(5432), hold: HOLD }));
  const stop = async () => { try { await c.stop(); } finally { process.exit(0); } };
  process.on("SIGTERM", stop);
  process.on("SIGINT", stop);
  setTimeout(stop, HOLD * 1000);
})().catch((e) => {
  console.error("FAIL", e.message);
  process.exit(1);
});
