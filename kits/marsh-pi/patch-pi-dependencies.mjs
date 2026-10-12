// Pi 1.0.0 embedded an npm-shrinkwrap pinning brace-expansion 5.0.9
// (GHSA-q2hr-2g5m-vwhr, GHSA-qhr7-859c-m2p7, GHSA-6j4f-fj2g-mc7p), which npm ci
// installs verbatim (root `overrides` and lock entries are ignored for it), so
// this script used to replace that nested copy. Pi 1.1.0 no longer nests
// brace-expansion: its minimatch resolves our separately locked fixed package.
// Keep the guard: reject unreviewed Pi or dependency versions, any
// reintroduced nested copy, and verify the module Pi's minimatch actually loads.
import { createRequire } from 'node:module';
import { existsSync, readFileSync, realpathSync } from 'node:fs';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const modules = join(root, 'node_modules');
const fixed = join(modules, 'brace-expansion');
const pi = join(modules, '@earendil-works/pi-coding-agent');
const nested = join(pi, 'node_modules/brace-expansion');
const version = path => JSON.parse(readFileSync(join(path, 'package.json'), 'utf8')).version;
if (version(pi) !== '1.1.0' || version(fixed) !== '5.0.12') {
  throw new Error('Pi dependency check requires reviewed Pi 1.1.0 and brace-expansion 5.0.12');
}
if (existsSync(nested)) {
  throw new Error('Pi bundles brace-expansion again; review the dependency repair');
}
const fromPi = createRequire(join(pi, 'package.json'));
const fromMinimatch = createRequire(fromPi.resolve('minimatch'));
const actual = realpathSync(fromMinimatch.resolve('brace-expansion'));
if (!actual.startsWith(realpathSync(resolve(fixed)) + sep)) {
  throw new Error('Pi minimatch did not resolve the fixed brace-expansion');
}
const { minimatch } = fromPi('minimatch');
if (!minimatch('src/file.rs', '{src,tests}/**/*.rs') || minimatch('secret.txt', '{src,tests}/**/*.rs')) {
  throw new Error('Pi glob behavior failed with the fixed brace-expansion');
}
console.log('Pi minimatch resolves brace-expansion 5.0.12; representative glob checks passed');
