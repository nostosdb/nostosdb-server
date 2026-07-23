#!/usr/bin/env node
// SPDX-License-Identifier: SSPL-1.0

"use strict";

const { runCli } = require("../lib/launcher.cjs");

runCli(process.argv.slice(2));
