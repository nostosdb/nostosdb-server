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
    manifestPath = resolvePackage("@nostosdb/cli/package.json");
  } catch (error) {
    throw new LauncherError(
      `missing @nostosdb/cli@${version}; reinstall @nostosdb/server@${version}`,
      { cause: error },
    );
  }
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  if (manifest.name !== "@nostosdb/cli" || manifest.version !== version) {
    throw new LauncherError(
      `CLI package mismatch: expected @nostosdb/cli@${version}, ` +
        `found ${manifest.name}@${manifest.version}`,
    );
  }
  const launcher = loadPackage("@nostosdb/cli");
  if (
    !launcher ||
    typeof launcher.launchBinary !== "function" ||
    typeof launcher.run !== "function"
  ) {
    throw new LauncherError("@nostosdb/cli does not expose the required launcher API");
  }
  return launcher;
}

function reportFailure(error) {
  console.error(`nostosdb server launcher: ${error.message}`);
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
