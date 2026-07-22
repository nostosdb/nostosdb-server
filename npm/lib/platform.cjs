// SPDX-License-Identifier: SSPL-1.0

"use strict";

const fs = require("node:fs");
const path = require("node:path");

const PLATFORM_PACKAGES = Object.freeze({
  "darwin-arm64": "@nostosdb/server-darwin-arm64",
  "darwin-x64": "@nostosdb/server-darwin-x64",
  "linux-arm64-gnu": "@nostosdb/server-linux-arm64-gnu",
  "linux-x64-gnu": "@nostosdb/server-linux-x64-gnu",
  "win32-arm64": "@nostosdb/server-win32-arm64",
  "win32-x64": "@nostosdb/server-win32-x64",
});

class PlatformError extends Error {}

function linuxLibc(report = process.report) {
  if (!report || typeof report.getReport !== "function") {
    throw new PlatformError("cannot determine Linux libc; GNU/glibc is required");
  }
  const details = report.getReport();
  const header = details && details.header;
  if (!header || !header.glibcVersionRuntime) {
    throw new PlatformError("unsupported Linux libc; GNU/glibc is required");
  }
  return "gnu";
}

function packageFor(platform, arch, report = process.report) {
  const suffix = platform === "linux" ? `-${linuxLibc(report)}` : "";
  const key = `${platform}-${arch}${suffix}`;
  const packageName = PLATFORM_PACKAGES[key];
  if (!packageName) {
    throw new PlatformError(`unsupported NostosDB Server platform: ${platform}-${arch}`);
  }
  return packageName;
}

function resolveBinary({
  platform = process.platform,
  arch = process.arch,
  version,
  resolvePackage = require.resolve,
  report = process.report,
}) {
  const packageName = packageFor(platform, arch, report);
  let manifestPath;
  try {
    manifestPath = resolvePackage(`${packageName}/package.json`);
  } catch (error) {
    throw new PlatformError(
      `missing optional package ${packageName}@${version}; reinstall @nostosdb/server@${version}`,
      { cause: error },
    );
  }
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  if (manifest.name !== packageName || manifest.version !== version) {
    throw new PlatformError(
      `platform package mismatch: expected ${packageName}@${version}, ` +
        `found ${manifest.name}@${manifest.version}`,
    );
  }
  const executable = platform === "win32" ? "nostosd.exe" : "nostosd";
  const binary = path.join(path.dirname(manifestPath), "bin", executable);
  let details;
  try {
    details = fs.statSync(binary);
  } catch (error) {
    throw new PlatformError(`platform package has no executable: ${binary}`, {
      cause: error,
    });
  }
  if (!details.isFile()) {
    throw new PlatformError(`platform executable is not a file: ${binary}`);
  }
  return binary;
}

module.exports = {
  PLATFORM_PACKAGES,
  PlatformError,
  linuxLibc,
  packageFor,
  resolveBinary,
};
