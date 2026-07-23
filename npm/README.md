# `@nostdb/server` candidate launcher

This unpublished SSPL-1.0 package installs the native `nostd` database daemon and the exact matching `@nostdb/cli`. One global install exposes both commands:

```bash
npm install --global @nostdb/server
nostd --version
nostdb --version
```

The JavaScript wrapper contains no Core, query, storage, or `.ndb` implementation. It validates the exact OS/CPU native Server package and delegates process execution through the matching CLI launcher's shell-free process boundary. It has no lifecycle downloader and does not initialize or start a daemon during installation.

No package in this directory is published by its tests or candidate scripts. See the repository [distribution contract](../docs/DATABASE_SERVER.md).
