# Agent instructions for nostos-server

Follow the Root `AGENTS.md` when working in the multi-repository workspace.

- This repository owns the network boundary and consumes the public `nostos-engine` facade.
- Do not duplicate the query engine, planner, synchronization, or storage.
- Keep every operational endpoint behind API-key authentication; only liveness may be public.
- Enforce query limits cooperatively through Engine APIs and preserve atomic rollback on cancellation.
- Verify snapshot compatibility before replacing the live database and keep logical packages distinct from snapshots.
- Use stable Rust and Edition 2024.
- Preserve the SSPL-1.0 source-available license assignment.
