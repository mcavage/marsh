import { Client } from "pg";

// Reads DATABASE_URL from the environment Next loads from .env (git-ignored).
export async function query(sql: string, values: unknown[] = []) {
  const client = new Client({ connectionString: process.env.DATABASE_URL });
  await client.connect();
  try {
    return (await client.query(sql, values)).rows;
  } finally {
    await client.end();
  }
}
