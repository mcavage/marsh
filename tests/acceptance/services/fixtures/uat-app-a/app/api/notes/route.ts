import { query } from "../../../lib/db";
import { VERSION } from "../../../lib/version";

export const dynamic = "force-dynamic";

// The old schema keeps the text in the `note` column. Dropping that column
// (J1: "drop the old note column") makes this endpoint fail until the code and
// the schema agree again.
export async function GET() {
  try {
    const notes = await query("select id, title, note from notes order by id");
    return Response.json({ version: VERSION, notes });
  } catch (error) {
    return Response.json({ version: VERSION, error: String((error as Error).message) }, { status: 500 });
  }
}
