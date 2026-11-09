// `npm run dev`: an HTTP server that reads DATABASE_URL from .env (the way a
// dev server does; the process environment is not consulted) and serves
// stack A's database. It never changes between the original and a fork: only
// the data behind localhost:5432 does.
//   GET /items   -> {"count": N, "row1": "<name of row 1>", "markers": [...]}
// (RST-13 sets a per-run nonce by UPDATING row 1, so the count stays 1000.)
//   GET /health  -> "ok"
const fs = require("fs");
const http = require("http");
const { Client } = require("pg");

function readEnv(path) {
  const out = {};
  for (const raw of fs.readFileSync(path, "utf8").split(/\r?\n/)) {
    const m = /^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$/.exec(raw);
    if (!m || raw.trim().startsWith("#")) continue;
    out[m[1]] = m[2].replace(/^(["'])(.*)\1$/, "$2");
  }
  return out;
}

const env = readEnv(".env");
if (!env.DATABASE_URL) {
  console.error("nfx-devserver: no DATABASE_URL in .env");
  process.exit(1);
}

async function items() {
  const db = new Client({ connectionString: env.DATABASE_URL });
  await db.connect();
  try {
    const count = (await db.query("select count(*)::int as n from items")).rows[0].n;
    const row1 = (await db.query("select name from items where id = 1")).rows[0]?.name ?? null;
    const markers = (await db.query("select name from markers order by name")).rows.map((r) => r.name);
    return { count, row1, markers };
  } finally {
    await db.end();
  }
}

http.createServer(async (req, res) => {
  try {
    if (req.url === "/health") return void res.end("ok");
    if (req.url === "/items") {
      res.setHeader("content-type", "application/json");
      return void res.end(JSON.stringify(await items()));
    }
    res.statusCode = 404;
    res.end("not found");
  } catch (e) {
    res.statusCode = 500;
    res.end(String(e.message));
  }
}).listen(Number(process.env.PORT || 3000), () => console.log("nfx-devserver listening"));
