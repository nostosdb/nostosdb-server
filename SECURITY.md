# Security policy for nostdb-server

There is no supported Server release or production security SLA. Current `main` is single-node evaluation software.

Privately report authentication bypass, request smuggling/parsing,
resource-limit escape, transaction isolation/timeout, snapshot or
logical-import validation, path handling, credential logging, or sensitive
metrics issues through **Security → Report a vulnerability**. Private reporting
was externally checked as enabled on 2026-07-23. If absent, request
re-enablement publicly without details and wait. Use synthetic requests/data and
redact keys.

Maintainers target acknowledgement in three business days and triage in seven, without an SLA or bounty. Deploy evaluation instances on loopback or behind independently configured TLS/auth controls.
