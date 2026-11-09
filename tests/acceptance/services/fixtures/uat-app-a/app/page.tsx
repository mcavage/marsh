import { query } from "../lib/db";
import { VERSION } from "../lib/version";

export const dynamic = "force-dynamic";

export default async function Page() {
  let notes: { id: number; title: string; note: string }[] = [];
  let failure = "";
  try {
    notes = await query("select id, title, note from notes order by id");
  } catch (error) {
    failure = (error as Error).message;
  }
  return (
    <main>
      <h1>Notes ({VERSION})</h1>
      {failure ? <p id="error">{failure}</p> : (
        <ul>{notes.map((n) => <li key={n.id}>{n.title}: {n.note}</li>)}</ul>
      )}
    </main>
  );
}
