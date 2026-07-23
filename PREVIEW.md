# Server preview status

The Server is source-available SSPL-1.0 single-node evaluation software with an implemented daemon and unpublished deployment candidates, but no supported release, installer, registered service, published image, or hosted service.

- `nostd` implements a managed data directory, atomic named-Database catalog, exclusive ownership, stateful database protocol, remote CLI, transactions, cancellation, limits, snapshots, logical packages, and restart recovery.
- Protocol version 1 has separate query/admin credentials and no TLS negotiation. It binds to loopback by default; credential rotation, Database-scoped grants, multi-tenant identity, and production remote transport are not implemented.
- Homebrew, systemd, Windows Service, Docker, and combined archive files are review candidates only. They have not been published, installed on a production host, or signed with production credentials.
- The single-file HTTP binary is transitional MCP compatibility behavior, not the product identity or an application API platform. Its API-key and `/healthz` rules are separate from the database protocol.
- Transactions and connections are bounded; clustering, replication, automatic failover, and HA are absent.
- Snapshot Format 0 compatibility is exact/experimental; logical packages are distinct.
- No production SLA, backup service, publication, or external contribution intake exists.

Bind to loopback, protect generated credential files, use disposable data, and retain independent backups.
