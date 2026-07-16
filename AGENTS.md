# Agent instructions for nostos-server

Follow the Root `AGENTS.md` when working in the multi-repository workspace.

- This repository owns the future network boundary and consumes the public `nostos-engine` facade.
- Do not duplicate the query engine, planner, synchronization, or storage.
- Stage 0 permits only a compiling process skeleton; HTTP and operational behavior are deferred to Stage 8.
- Use stable Rust and Edition 2024.
- Preserve the SSPL-1.0 source-available license assignment.
