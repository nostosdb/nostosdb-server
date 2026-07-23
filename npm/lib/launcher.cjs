// SPDX-License-Identifier: SSPL-1.0

"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { resolveBinary } = require("./platform.cjs");

class LauncherError extends Error {}

function launcherVersion() {
  const manifest = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8"),
  );
  return manifest.version;
}

function resolveCliLauncher({
  version,
  resolvePackage = require.resolve,
  loadPackage = require,
}) {
  let manifestPath;
  try {
    manifestPath = resolvePackage("@nostdb/cli/package.json");
  } catch (error) {
    throw new LauncherError(
      `missing @nostdb/cli@${version}; reinstall @nostdb/server@${version}`,
      { cause: error },
    );
  }
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  if (manifest.name !== "@nostdb/cli" || manifest.version !== version) {
    throw new LauncherError(
      `CLI package mismatch: expected @nostdb/cli@${version}, ` +
        `found ${manifest.name}@${manifest.version}`,
    );
  }
  const launcher = loadPackage("@nostdb/cli");
  if (
    !launcher ||
    typeof launcher.launchBinary !== "function" ||
    typeof launcher.run !== "function"
  ) {
    throw new LauncherError("@nostdb/cli does not expose the required launcher API");
  }
  return launcher;
}

function reportFailure(error) {
  console.error(`nostdb server launcher: ${error.message}`);
  process.exitCode = 3;
  return null;
}

function runDaemon(arguments_, options = {}) {
  try {
    const version = options.version || launcherVersion();
    const binary = (options.resolveBinary || resolveBinary)({
      ...(options.platform || {}),
      version,
    });
    const cliLauncher =
      options.cliLauncher ||
      resolveCliLauncher({
        ...(options.cli || {}),
        version,
      });
    return cliLauncher.launchBinary(binary, arguments_, options.launchOptions);
  } catch (error) {
    return reportFailure(error);
  }
}

function runCli(arguments_, options = {}) {
  try {
    const version = options.version || launcherVersion();
    const cliLauncher =
      options.cliLauncher ||
      resolveCliLauncher({
        ...(options.cli || {}),
        version,
      });
    return cliLauncher.run(arguments_);
  } catch (error) {
    return reportFailure(error);
  }
}

module.exports = {
  LauncherError,
  launcherVersion,
  resolveCliLauncher,
  runCli,
  runDaemon,
};
