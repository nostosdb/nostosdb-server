# Security policy for nostosdb-server

There is no supported Server release or production security SLA. Current `main` is single-node evaluation software.

Privately report authentication bypass, request smuggling/parsing, resource-limit escape, transaction isolation/timeout, snapshot or logical-import validation, path handling, credential logging, or sensitive metrics issues through **Security → Report a vulnerability** after private reporting is enabled. If absent, request enablement publicly without details and wait. Use synthetic requests/data and redact keys.

Maintainers target acknowledgement in three business days and triage in seven, without an SLA or bounty. Deploy evaluation instances on loopback or behind independently configured TLS/auth controls.
