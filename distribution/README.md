# Database daemon distribution candidates

These files are review candidates and perform no publication, service registration, or image push by themselves.

- `release-manifest.json`, `scripts/stage_npm_candidate.py`, and the `npm/` package tree define the unpublished `@nostosdb/server` launcher plus six exact native packages. `scripts/verify_local_npm.py` proves an offline isolated global install exposes both `nostosd` and `nostos` through the exact matching CLI package.
- `systemd/nostosdb.service` runs `/usr/local/bin/nostosd serve --config /etc/nostosdb/server.toml` as a dedicated account with `/var/lib/nostosdb` as its only writable database state.
- `homebrew/Formula/nostosdb.rb.in` defines formula/service name `nostosdb`, installs the combined `nostos`/`nostosd` candidate, initializes loopback-only per-user state under `~/.nostosdb` once, and exposes `nostosd` through `brew services`.
- `windows/install-service.ps1` creates a Windows Service only when an operator explicitly runs it. It refuses to replace an existing service and uses `%PROGRAMDATA%\NostosDB\server.toml`.
- `Dockerfile` and `compose.yaml` build from the NostosDB root context and use `/etc/nostosdb/server.toml` plus the `/var/lib/nostosdb` authoritative volume.

All service forms execute the same versioned configuration, catalog, credentials, and data-directory runtime. Installation scripts must restrict the service identity and credential files for their platform before production use.
