// Run with the pinned renovate-config-validator executable as argv[2]. Use
// Renovate's real extraction and rule matching so display names cannot silently
// stop the publisher's Python compatibility constraint from applying.
import assert from 'node:assert/strict';
import { readFileSync, realpathSync } from 'node:fs';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';

const validator = pathToFileURL(realpathSync(process.argv[2]));
const require = createRequire(validator);
const JSON5 = require('json5');
const { GlobalConfig } = await import(new URL('./config/global.js', validator));
const { init } = await import(new URL('./logger/index.js', validator));
await init();
GlobalConfig.set({ localDir: process.cwd(), platform: 'github' });
const { extractPackageFile } = await import(new URL('./modules/manager/github-actions/extract.js', validator));
const { applyPackageRules } = await import(new URL('./util/package-rules/index.js', validator));
const { api: versions } = await import(new URL('./modules/versioning/npm/index.js', validator));
const config = JSON5.parse(readFileSync('.github/renovate.json5', 'utf8'));
const packageFile = '.github/workflows/ci.yml';
const extracted = await extractPackageFile(readFileSync(packageFile, 'utf8'), packageFile);
const python = extracted.deps.find(dep => dep.depName === 'python');
assert.ok(python, 'Renovate must find the publisher Python interpreter');
const policy = await applyPackageRules({ ...python, manager: 'github-actions', packageFile, packageRules: config.packageRules });
assert.ok(policy.allowedVersions, 'Publisher Python must have a compatibility constraint');
assert.ok(versions.subset(python.currentValue, policy.allowedVersions), 'Configured publisher Python must satisfy its constraint');
assert.ok(versions.matches('3.14.10', policy.allowedVersions), 'Python 3.14 must remain eligible');
assert.ok(!versions.matches('3.15.0', policy.allowedVersions), 'Python 3.15 has no pinned PyYAML wheel');
assert.ok(!versions.matches('4.0.0', policy.allowedVersions), 'Future incompatible Python majors must remain excluded');
const action = extracted.deps.find(dep => dep.depName === 'actions/setup-python');
assert.ok(action, 'The setup-python action must still be extracted');
const actionPolicy = await applyPackageRules({ ...action, manager: 'github-actions', packageFile, packageRules: config.packageRules });
assert.equal(actionPolicy.allowedVersions, undefined, 'The interpreter constraint must not restrict the action version');
console.log('Renovate publisher Python policy passed');
