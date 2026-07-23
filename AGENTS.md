# Agent instructions for nostdb-server

Follow the Root `AGENTS.md` when working in the multi-repository workspace.

- This repository owns the installable `nostd` database-daemon, managed data-directory, Database catalog, and network boundary. It consumes the public `nostdb-engine` facade.
- Do not duplicate the query engine, planner, synchronization, or storage.
- Treat Database names and stable Database IDs as client-visible identity; never expose a managed filesystem path as Database identity.
- While the current HTTP compatibility transport exists, keep every operational endpoint behind authentication; only liveness may be public.
- Enforce query limits cooperatively through Engine APIs and preserve atomic rollback on cancellation.
- Verify snapshot compatibility before replacing the live database and keep logical packages distinct from snapshots.
- Do not describe the HTTP transport as an application REST API product. Database protocol, daemon lifecycle, service installation, and container behavior must follow `docs/DATABASE_SERVER.md` and `docs/DATABASE_PROTOCOL.md`.
- Use stable Rust and Edition 2024.
- Preserve the SSPL-1.0 source-available license assignment.
