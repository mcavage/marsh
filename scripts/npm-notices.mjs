// Copied unchanged into each standalone npm Kit build context.
// Inventory installed modules after all dependency repairs, never just the lock.
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, lstatSync, mkdirSync, readFileSync, readdirSync, realpathSync, writeFileSync } from 'node:fs';
import { dirname, extname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const modules = resolve(process.argv[2] ?? join(here, 'node_modules'));
const output = resolve(process.argv[3] ?? join(here, 'installed-notices'));
const overrides = JSON.parse(readFileSync(join(here, 'notices/overrides.json'), 'utf8'));
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const rulesPath = '/usr/local/share/licenses/marsh-dhi/notice-rules.json';
const rulesStat = lstatSync(rulesPath);
if (!rulesStat.isFile() || rulesStat.size > 65536 || realpathSync(rulesPath) !== rulesPath) {
  throw new Error('Stage the bounded canonical notice vocabulary before collection');
}
const rules = JSON.parse(readFileSync(rulesPath, 'utf8'));
if (rules.schema !== 'marsh.notice-vocabulary/v1') throw new Error('Unknown notice vocabulary');
const noticeName = new RegExp(rules.name_pattern, 'i');
const codeSuffixes = new Set(rules.code_suffixes);
const noticeDirectories = new Set(rules.notice_directories);
const dataSuffixes = new Set(rules.data_suffixes);
const fixtureDirectories = new Set(rules.fixture_directories);
const fixturePaths = rules.fixture_path_components;
function noticeDocument(path, directory) {
  const parts = relative(directory, path).split(sep);
  return !codeSuffixes.has(extname(path).toLowerCase()) &&
    !dataSuffixes.has(extname(path).toLowerCase()) &&
    !parts.slice(0, -1).some(part => fixtureDirectories.has(part.toLowerCase())) &&
    !fixturePaths.some(path => parts.slice(0, -path.length).some((_, i) =>
      path.every((part, j) => parts[i + j].toLowerCase() === part))) &&
    (noticeName.test(parts.at(-1).replace(/^\.+/, '')) ||
     parts.slice(0, -1).some(part => noticeDirectories.has(part.toLowerCase())));
}
const packages = [];
const seen = new Set();
mkdirSync(join(output, 'texts'), { recursive: true });

function save(bytes, source, provenance = null) {
  const sha256 = hash(bytes);
  writeFileSync(join(output, 'texts', sha256 + '.txt'), bytes);
  return { source, sha256, provenance };
}

function packageNotices(directory, current = directory, depth = 0) {
  if (depth > 12) throw new Error('Notice directory depth exceeds bound');
  const result = [];
  for (const entry of readdirSync(current, { withFileTypes: true })) {
    if (entry.name === 'node_modules' || entry.name === '.git') continue;
    const path = join(current, entry.name);
    if (entry.isDirectory()) result.push(...packageNotices(directory, path, depth + 1));
    else if (entry.isFile() && noticeDocument(path, directory)) {
      if (lstatSync(path).size > 16 * 1024 * 1024) throw new Error('Notice exceeds byte bound');
      result.push(save(readFileSync(path), relative(directory, path)));
    }
  }
  return result;
}

function inspect(directory) {
  const canonical = realpathSync(directory);
  if (!canonical.startsWith(modules + sep)) throw new Error('Package escapes module root');
  if (seen.has(canonical)) return;
  seen.add(canonical);
  const manifest = JSON.parse(readFileSync(join(directory, 'package.json'), 'utf8'));
  if (!manifest.name || !manifest.version) throw new Error('Package lacks identity');
  const notices = packageNotices(directory);
  if (!notices.length) {
    for (const name of ['README.md', 'readme.md', 'README']) {
      const path = join(directory, name);
      if (existsSync(path)) {
        const data = readFileSync(path);
        if (data.includes(Buffer.from('Permission is hereby granted'))) {
          notices.push(save(data, name));
          break;
        }
      }
    }
  }
  for (const item of overrides[manifest.name + '@' + manifest.version] ?? []) {
    let source = join(here, 'notices', item.file);
    const canonicalFile = overrides.$canonical_notice_files?.[item.file];
    if (canonicalFile !== undefined) {
      // Canonical preparation stages this tree once. The Kit maps names, not
      // legal bytes. Arbitrary absolute paths and traversal are never admitted.
      if (!/^(provider-notices|texts)\/[A-Za-z0-9][A-Za-z0-9._-]{0,255}$/.test(canonicalFile)) {
        throw new Error('Invalid canonical notice mapping');
      }
      const canonicalRoot = '/usr/local/share/licenses/marsh-dhi';
      source = join(canonicalRoot, canonicalFile);
      if (canonicalFile.startsWith('provider-notices/')) {
        const index = JSON.parse(readFileSync(join(canonicalRoot, 'agent-notices.json'), 'utf8'));
        if (!index.notices.some(row => 'provider-notices/' + row.file === canonicalFile && row.sha256 === item.sha256)) {
          throw new Error('Canonical provider notice is not indexed at the required hash');
        }
      } else if (canonicalFile !== 'texts/' + item.sha256 + '.txt') {
        throw new Error('Canonical notice content address does not match override');
      }
      const metadata = lstatSync(source);
      if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > 16 * 1024 * 1024 || realpathSync(source) !== source) {
        throw new Error('Unsafe canonical provider notice');
      }
    }
    const bytes = readFileSync(source);
    if (hash(bytes) !== item.sha256) throw new Error('Notice override hash changed');
    notices.push(save(bytes, item.file, item));
  }
  if (!notices.length) throw new Error(`Missing notice text: ${manifest.name}@${manifest.version}`);
  packages.push({ name: manifest.name, version: manifest.version,
    path: relative(modules, directory), license: manifest.license ?? null,
    author: manifest.author ?? null, repository: manifest.repository ?? null,
    package_json_sha256: hash(readFileSync(join(directory, 'package.json'))), notices });
  scanModules(join(directory, 'node_modules'));
}

function scanModules(path) {
  if (!existsSync(path)) return;
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    if (entry.name.startsWith('.')) continue;
    const directory = join(path, entry.name);
    if (!(entry.isDirectory() || entry.isSymbolicLink())) continue;
    if (entry.name.startsWith('@')) {
      for (const name of readdirSync(directory)) inspect(join(directory, name));
    } else inspect(directory);
  }
}

scanModules(modules);
packages.sort((a, b) => a.path.localeCompare(b.path));
const lock = readFileSync(join(here, 'package-lock.json'));
writeFileSync(join(output, 'npm-package-notices.json'), JSON.stringify({
  schema: 'marsh.installed-npm-notices.v1', package_lock_sha256: hash(lock),
  architecture: process.arch, platform: process.platform, packages,
}, null, 2) + '\n');
copyFileSync(join(here, 'notices/overrides.json'), join(output, 'notice-overrides.json'));
writeFileSync(join(output, 'README.txt'),
  'Installed npm package inventory after dependency repairs. Texts are indexed by SHA-256.\n' +
  'Original package notices remain in node_modules. SPDX declarations, authors and upstream\n' +
  'notice provenance are preserved. A missing upstream copyright statement is not invented.\n' +
  'This inventory does not grant provider service or binary redistribution rights.\n');
console.log(`Preserved notices for ${packages.length} installed npm packages`);
