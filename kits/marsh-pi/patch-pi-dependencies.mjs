// Pi 1.0.0 still embeds an npm-shrinkwrap pinning brace-expansion 5.0.9
// (GHSA-q2hr-2g5m-vwhr, GHSA-qhr7-859c-m2p7, GHSA-6j4f-fj2g-mc7p). npm ci
// installs a dependency's shrinkwrap verbatim: root `overrides` and a root
// lock entry are both ignored for it. Replace the nested package with a real
// copy (not a symlink: the notice verifier rejects symlinked package
// ancestors) of our separately locked fixed package, then verify the module
// that Pi's minimatch actually loads.
import { createRequire } from 'node:module';
import { cpSync, readFileSync, realpathSync, rmSync } from 'node:fs';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const modules = join(root, 'node_modules');
const fixed = join(modules, 'brace-expansion');
const pi = join(modules, '@earendil-works/pi-coding-agent');
const nested = join(pi, 'node_modules/brace-expansion');
const version = path => JSON.parse(readFileSync(join(path, 'package.json'), 'utf8')).version;
if (version(pi) !== '1.0.0' || version(fixed) !== '5.0.12') {
  throw new Error('Pi dependency repair requires reviewed Pi 1.0.0 and brace-expansion 5.0.12');
}
if (version(nested) !== '5.0.9' && version(nested) !== '5.0.12') {
  throw new Error('Unexpected bundled brace-expansion; review the dependency repair');
}
rmSync(nested, { recursive: true, force: true });
cpSync(fixed, nested, { recursive: true, verbatimSymlinks: true });
const fromPi = createRequire(join(pi, 'package.json'));
const fromMinimatch = createRequire(fromPi.resolve('minimatch'));
const actual = realpathSync(fromMinimatch.resolve('brace-expansion'));
if (!actual.startsWith(resolve(nested) + sep) || version(nested) !== '5.0.12') {
  throw new Error('Pi minimatch did not resolve the fixed brace-expansion');
}
const { minimatch } = fromPi('minimatch');
if (!minimatch('src/file.rs', '{src,tests}/**/*.rs') || minimatch('secret.txt', '{src,tests}/**/*.rs')) {
  throw new Error('Pi glob behavior failed after the dependency repair');
}
console.log('Pi minimatch resolves brace-expansion 5.0.12; representative glob checks passed');
