// SPDX-License-Identifier: SSPL-1.0

"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { PLATFORM_PACKAGES } = require("../lib/platform.cjs");

const root = path.resolve(__dirname, "..");
const launcher = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
const releaseManifest = JSON.parse(
  fs.readFileSync(
    path.join(root, "..", "distribution", "release-manifest.json"),
    "utf8",
  ),
);
const expectedDirectories = Object.values(PLATFORM_PACKAGES)
  .map((name) => name.replace("@nostosdb/server-", ""))
  .sort();
const actualDirectories = fs
  .readdirSync(path.join(root, "packages"), { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name)
  .sort();

assert.deepEqual(actualDirectories, expectedDirectories);
assert.equal(launcher.name, "@nostosdb/server");
assert.equal(launcher.license, "SSPL-1.0");
assert.equal(launcher.bin.nostosd, "bin/nostosd.js");
assert.equal(launcher.bin.nostos, "bin/nostos.js");
assert.deepEqual(launcher.dependencies, { "@nostosdb/cli": launcher.version });
assert.equal(launcher.scripts.preinstall, undefined);
assert.equal(launcher.scripts.install, undefined);
assert.equal(launcher.scripts.postinstall, undefined);
assert.equal(launcher.publishConfig.access, "public");
assert.equal(releaseManifest.version, launcher.version);
assert.equal(releaseManifest.binary, "nostosd");
assert.equal(
  launcher.repository.url,
  "git+https://github.com/nostosdb/nostosdb-server.git",
);

for (const directory of actualDirectories) {
  const manifestPath = path.join(root, "packages", directory, "package.json");
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  assert.equal(manifest.name, `@nostosdb/server-${directory}`);
  assert.equal(manifest.version, launcher.version);
  assert.equal(launcher.optionalDependencies[manifest.name], launcher.version);
  assert.equal(manifest.license, "SSPL-1.0");
  assert.equal(manifest.scripts, undefined);
  assert.equal(manifest.publishConfig.access, "public");
  assert.deepEqual(manifest.os, [directory.split("-")[0]]);
  assert.deepEqual(manifest.cpu, [directory.split("-")[1]]);
  if (directory.startsWith("linux-")) {
    assert.deepEqual(manifest.libc, ["glibc"]);
  } else {
    assert.equal(manifest.libc, undefined);
  }
}

assert.deepEqual(
  Object.values(releaseManifest.targets)
    .map((target) => target.npm_package)
    .sort(),
  Object.values(PLATFORM_PACKAGES).sort(),
);

const runtimeFiles = [
  path.join(root, "bin", "nostos.js"),
  path.join(root, "bin", "nostosd.js"),
  path.join(root, "lib", "launcher.cjs"),
  path.join(root, "lib", "platform.cjs"),
];
const forbidden = [
  "child_process",
  "sqlite",
  "nostos-parser",
  "nostos-storage",
  "writeFileSync",
  "writeFile",
  ".ndb",
  "npm publish",
];
for (const file of runtimeFiles) {
  const source = fs.readFileSync(file, "utf8");
  for (const marker of forbidden) {
    assert.equal(source.includes(marker), false, `${file} contains forbidden ${marker}`);
  }
}

console.log(
  `verified @nostosdb/server launcher and ${actualDirectories.length} platform manifests`,
);
