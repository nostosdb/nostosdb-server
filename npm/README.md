# `@nostdb/server` candidate launcher

This directory describes the unreleased `0.0.3` candidate. The published
SSPL-1.0 `@nostdb/server@0.0.2` package installs the native `nostd` database
daemon and exact matching `@nostdb/cli@0.0.2`. One global install exposes both
commands:

```bash
npm install --global @nostdb/server
nostd --version
nostdb --version
```

The JavaScript wrapper contains no Core, query, storage, or `.nostdb` implementation. It validates the exact OS/CPU native Server package and delegates process execution through the matching CLI launcher's shell-free process boundary. It has no lifecycle downloader and does not initialize or start a daemon during installation.

Tests and candidate scripts in this directory never publish by themselves. See
the repository [distribution contract](../docs/DATABASE_SERVER.md).
