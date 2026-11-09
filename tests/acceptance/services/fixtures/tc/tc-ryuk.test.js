// Ryuk-on path: like tc.test.js, and it fails unless Testcontainers really
// started a Ryuk (reaper) container whose socket mount it reports. Run it with
// the job's default environment (no TESTCONTAINERS_RYUK_DISABLED).
const { IMAGE } = require("./tc-common");
const { GenericContainer, Wait, getContainerRuntimeClient } = require("testcontainers");
const { Client } = require("pg");

(async () => {
  if (process.env.TESTCONTAINERS_RYUK_DISABLED === "true") throw new Error("Ryuk is disabled in this environment");
  const c = await new GenericContainer(IMAGE)
    .withEnvironment({ POSTGRES_PASSWORD: "tc" })
    .withExposedPorts(5432)
    .withWaitStrategy(Wait.forLogMessage(/ready to accept connections/, 2))
    .start();
  const pg = new Client({ host: c.getHost(), port: c.getMappedPort(5432), user: "postgres", password: "tc" });
  await pg.connect();
  const answer = (await pg.query("select 41 + 1 as answer")).rows[0].answer;
  await pg.end();

  const docker = (await getContainerRuntimeClient()).container.dockerode;
  const ryuk = (await docker.listContainers()).filter((x) => x.Image.includes("ryuk"));
  const mounts = ryuk.flatMap((x) => x.Mounts.map((m) => `${m.Source}:${m.Destination}`));
  const ok = answer === 42 && ryuk.length >= 1;
  console.log(JSON.stringify({ ok, answer, ryuk: ryuk.map((x) => ({ id: x.Id, image: x.Image })), ryuk_mounts: mounts }));
  await c.stop();
  if (!ok) process.exit(1);
})().catch((e) => {
  console.error("FAIL", e.message);
  process.exit(1);
});
