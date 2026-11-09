const { Client } = require("pg");

async function withClient(url, fn) {
  const client = new Client({ connectionString: url });
  await client.connect();
  try {
    return await fn(client);
  } finally {
    await client.end();
  }
}

exports.addUser = (url, name) => withClient(url, (c) => c.query("insert into users (name) values ($1) returning id", [name]).then((r) => r.rows[0].id));
exports.countUsers = (url) => withClient(url, (c) => c.query("select count(*)::int as n from users").then((r) => r.rows[0].n));
exports.schema = "create table if not exists users (id serial primary key, name text not null)";
exports.migrate = (url) => withClient(url, (c) => c.query(exports.schema));
