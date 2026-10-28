// Usage: node skill-sources.mjs SETTINGS_JSON SOURCE...
//
// Prints one path per SOURCE, in order: the source itself, or a per-run folder
// of links to its skills without the ones a folder in the `skills` setting of
// SETTINGS_JSON already provides. Pi loads a settings folder ahead of `--skill`
// sources and keeps the first skill of each name, so this only drops the copies
// it would skip anyway, without Pi's collision warning: a skill in a configured
// folder (a dev checkout of the Kit's skills) replaces the Kit's copy quietly.
// A copy anywhere else (~/.agents/skills) still reaches Pi and still warns.
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, symlinkSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";

function skillName(file, fallback) {
  const text = readFileSync(file, "utf8");
  const front = text.startsWith("---") ? text.split(/\r?\n---/, 1)[0] : "";
  const match = front.match(/^name:\s*["']?([^"'\r\n]+?)["']?\s*$/m);
  return match ? match[1] : fallback;
}

// The skills in one folder, as Pi finds them: name -> entry in the folder.
function skills(folder) {
  const found = new Map();
  let entries;
  try {
    entries = readdirSync(folder, { withFileTypes: true });
  } catch {
    return found;
  }
  for (const entry of entries) {
    if (entry.name.startsWith(".") || entry.name === "node_modules") continue;
    const path = join(folder, entry.name);
    let info;
    try {
      info = statSync(path);
    } catch {
      continue;
    }
    if (info.isDirectory() && existsSync(join(path, "SKILL.md"))) {
      found.set(skillName(join(path, "SKILL.md"), entry.name), entry.name);
    } else if (info.isFile() && entry.name.endsWith(".md")) {
      found.set(skillName(path, basename(entry.name, ".md")), entry.name);
    }
  }
  return found;
}

function configuredFolders(settingsFile) {
  let settings;
  try {
    settings = JSON.parse(readFileSync(settingsFile, "utf8"));
  } catch {
    return [];
  }
  if (!Array.isArray(settings?.skills)) return [];
  return settings.skills
    .filter((entry) => typeof entry === "string" && entry && !"!+-".includes(entry[0]))
    .map((entry) => (entry === "~" || entry.startsWith("~/") ? join(homedir(), entry.slice(1)) : entry))
    .map((entry) => (isAbsolute(entry) ? entry : resolve(dirname(settingsFile), entry)));
}

const [settingsFile, ...sources] = process.argv.slice(2);
const provided = new Set();
for (const folder of configuredFolders(settingsFile)) {
  for (const name of skills(folder).keys()) provided.add(name);
}
let run;
for (const source of sources) {
  const kept = [...skills(source)].filter(([name]) => !provided.has(name));
  if (kept.length === skills(source).size) {
    console.log(source);
    continue;
  }
  run ??= mkdtempSync(join(tmpdir(), "pix-skills-"));
  const folder = join(run, String(sources.indexOf(source)));
  mkdirSync(folder);
  for (const [, entry] of kept) symlinkSync(join(source, entry), join(folder, entry));
  console.log(folder);
}
