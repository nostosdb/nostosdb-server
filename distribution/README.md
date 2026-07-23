# Database daemon distribution candidates

These files are review candidates and perform no publication, service
registration, or image push by themselves. The npm `0.0.1` artifacts assembled
from this source were separately authorized and published.

- `release-manifest.json`, `scripts/stage_npm_candidate.py`, and the `npm/` package tree define the published `@nostdb/server@0.0.1` launcher plus six exact native packages. `scripts/verify_local_npm.py` proves an offline isolated global install exposes both `nostd` and `nostdb` through the exact matching CLI package.
- `systemd/nostdb.service` runs `/usr/local/bin/nostd serve --config /etc/nostdb/server.toml` as a dedicated account with `/var/lib/nostdb` as its only writable database state. Its [initialization procedure](systemd/README.md) runs `nostd init` as that account so generated `0600` credentials are readable by the service.
- `homebrew/Formula/nostdb.rb.in` defines formula/service name `nostdb`, installs the combined `nostdb`/`nostd` candidate, and exposes `nostd` through `brew services`. Its caveat pre-creates per-user `data`, `config`, and `logs` directories with mode `0700` before explicit loopback-only initialization; Homebrew's temporary `post_install` HOME must never hold persistent state.
- `windows/install-service.ps1` fails closed with an explicit unsupported diagnostic. The current `nostd.exe` is a foreground console process; it has no Windows Service Control Manager entry point or reviewed credential ACL installer and must not be registered directly with `sc.exe`.
- `Dockerfile` and `compose.yaml` build from the NostDB root context and use `/etc/nostdb/server.toml` plus the `/var/lib/nostdb` authoritative volume.

All service forms execute the same versioned configuration, catalog, credentials, and data-directory runtime. Installation scripts must restrict the service identity and credential files for their platform before production use.
