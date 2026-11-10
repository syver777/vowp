#!/usr/bin/env node
"use strict";

const { install } = require("../lib/install");

install().catch((error) => {
  process.stderr.write(`vowp: error: ${error.message}\n`);
  process.exitCode = 1;
});
