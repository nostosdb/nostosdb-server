#!/usr/bin/env node
// SPDX-License-Identifier: SSPL-1.0

"use strict";

const { runDaemon } = require("../lib/launcher.cjs");

runDaemon(process.argv.slice(2));
