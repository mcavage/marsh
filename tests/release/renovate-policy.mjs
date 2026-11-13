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
const { extractPackageFile: extractRegex } = await import(new URL('./modules/manager/custom/regex/index.js', validator));
const config = JSON5.parse(readFileSync('.github/renovate.json5', 'utf8'));
const engineFile = '.github/renovate-version';
const engineManager = config.customManagers.find(manager => manager.depNameTemplate === 'renovate');
const engine = extractRegex(readFileSync(engineFile, 'utf8'), engineFile, engineManager).deps[0];
assert.equal(engine.currentValue, require('renovate/package.json').version, 'Policy tests must use the bot\'s exact Renovate version');
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
const nodeDependencies = extracted.deps.filter(dep => dep.depName === 'node' && dep.depType === 'uses-with');
assert.ok(nodeDependencies.length, 'Renovate must find the CI Node interpreters');
for (const dep of nodeDependencies) {
  const nodePolicy = await applyPackageRules({ ...dep, manager: 'github-actions', packageFile, packageRules: config.packageRules });
  assert.ok(nodePolicy.allowedVersions, 'CI Node must have a compatibility constraint');
  assert.ok(versions.subset(dep.currentValue, nodePolicy.allowedVersions), 'Configured CI Node must satisfy its constraint');
  assert.ok(versions.intersects(dep.currentValue, require('renovate/package.json').engines.node), 'The validator must run on a Node version supported by the Renovate engine');
  assert.ok(versions.matches('24.22.2', nodePolicy.allowedVersions), 'Node 24 must remain eligible');
  assert.ok(!versions.matches('26.0.0', nodePolicy.allowedVersions), 'Node 26 requires a coordinated toolchain migration');
}
// The shell image's interpreter and library packages have exact-version
// dependencies. Verify that Renovate sees every pin and groups them with the
// base-image update, rather than proposing incompatible pieces separately.
const dockerfile = 'packaging/shell/Dockerfile';
const aptManager = config.customManagers.find(manager => manager.datasourceTemplate === 'deb');
const packages = extractRegex(readFileSync(dockerfile, 'utf8'), dockerfile, aptManager).deps;
const family = ['python-3.14', 'python-3.14-minimal', 'libpython-3.14', 'libpython-3.14-minimal', 'libpython-3.14-stdlib'];
const familyPins = family.map(name => {
  const dep = packages.find(item => item.depName === name);
  assert.ok(dep, `Renovate must discover the ${name} pin`);
  return dep.currentValue;
});
assert.equal(new Set(familyPins).size, 1, 'The Python runtime family must use one revision');
const imageFile = 'packaging/shell-image';
const imageManager = config.customManagers.find(manager => manager.managerFilePatterns.includes('/^packaging/shell-image$/'));
const image = extractRegex(readFileSync(imageFile, 'utf8'), imageFile, imageManager).deps[0];
const imagePolicy = await applyPackageRules({ ...image, packageName: image.packageName ?? image.depName, manager: 'custom.regex', packageFile: imageFile, packageRules: config.packageRules });
assert.ok(imagePolicy.groupName, 'DHI image updates must be grouped');
for (const dep of packages) {
  const packagePolicy = await applyPackageRules({ ...dep, packageName: dep.packageName ?? dep.depName, manager: 'custom.regex', packageFile: dockerfile, packageRules: config.packageRules });
  assert.equal(packagePolicy.groupName, imagePolicy.groupName, `${dep.depName} must update with the DHI base`);
}
// The services engine's Compose pin is the one Compose version; its copies
// are finished by hand (tests/release/test_compose_pins.py), so it must not
// hold back the DHI group. Its other apt pins stay with the DHI base.
const engineDockerfile = 'packaging/svc-engine/Dockerfile';
const enginePackages = extractRegex(readFileSync(engineDockerfile, 'utf8'), engineDockerfile, aptManager).deps;
const compose = enginePackages.filter(dep => dep.depName === 'docker-compose-plugin');
assert.equal(compose.length, 1, 'Renovate must discover the engine\'s one Compose pin');
for (const dep of enginePackages) {
  const packagePolicy = await applyPackageRules({ ...dep, packageName: dep.packageName ?? dep.depName, manager: 'custom.regex', packageFile: engineDockerfile, packageRules: config.packageRules });
  if (dep.depName === 'docker-compose-plugin') {
    assert.equal(packagePolicy.groupName, 'Docker Compose', 'The Compose pin must update in its own PR');
  } else {
    assert.equal(packagePolicy.groupName, imagePolicy.groupName, `${dep.depName} must update with the DHI base`);
  }
}
console.log('Renovate dependency policy passed');
