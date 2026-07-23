// SPDX-License-Identifier: SSPL-1.0

"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");
const {
  resolveCliLauncher,
  runCli,
  runDaemon,
} = require("../lib/launcher.cjs");

test("requires the exact matching CLI package and API", (context) => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "nostdb-cli-package-test-"));
  context.after(() => fs.rmSync(temporary, { recursive: true, force: true }));
  const manifestPath = path.join(temporary, "package.json");
  const expected = { launchBinary() {}, run() {} };
  fs.writeFileSync(
    manifestPath,
    JSON.stringify({ name: "@nostdb/cli", version: "0.0.1" }),
  );
  assert.equal(
    resolveCliLauncher({
      version: "0.0.1",
      resolvePackage(request) {
        assert.equal(request, "@nostdb/cli/package.json");
        return manifestPath;
      },
      loadPackage(request) {
        assert.equal(request, "@nostdb/cli");
        return expected;
      },
    }),
    expected,
  );
  assert.throws(
    () =>
      resolveCliLauncher({
        version: "0.2.0",
        resolvePackage: () => manifestPath,
        loadPackage: () => expected,
      }),
    /CLI package mismatch/,
  );
});

test("delegates nostd and nostdb without changing arguments", () => {
  const calls = [];
  const cliLauncher = {
    launchBinary(binary, arguments_, options) {
      calls.push({ arguments_, binary, options });
      return "daemon-child";
    },
    run(arguments_) {
      calls.push({ arguments_, command: "nostdb" });
      return "cli-child";
    },
  };
  const arguments_ = ["value with spaces", ";not-shell"];
  const launchOptions = { stdio: "inherit" };
  assert.equal(
    runDaemon(arguments_, {
      version: "0.0.1",
      resolveBinary: ({ version }) => {
        assert.equal(version, "0.0.1");
        return "/native/nostd";
      },
      cliLauncher,
      launchOptions,
      platform: { version: "9.9.9" },
    }),
    "daemon-child",
  );
  assert.equal(
    runCli(arguments_, { version: "0.0.1", cliLauncher }),
    "cli-child",
  );
  assert.deepEqual(calls, [
    { arguments_, binary: "/native/nostd", options: launchOptions },
    { arguments_, command: "nostdb" },
  ]);
});
