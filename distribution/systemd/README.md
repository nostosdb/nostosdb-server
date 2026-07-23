# systemd candidate initialization

The unit runs as the dedicated `nostdb` account. Initialize state as that same account so the daemon can read the generated `0600` credential files; running `nostd init` as `root` and leaving the credentials root-owned makes `serve` fail closed.

After installing the `nostdb` system account, binaries, and unit, initialize once:

```bash
sudo install -d -o nostdb -g nostdb -m 0700 /var/lib/nostdb
sudo install -d -o nostdb -g nostdb -m 0700 /etc/nostdb
sudo --user=nostdb -- /usr/local/bin/nostd init \
  --data-dir /var/lib/nostdb \
  --config /etc/nostdb/server.toml \
  --listen 127.0.0.1:7878
sudo chown root:nostdb /etc/nostdb/server.toml
sudo chmod 0640 /etc/nostdb/server.toml
sudo chown root:nostdb /etc/nostdb
sudo chmod 0750 /etc/nostdb
sudo systemctl enable --now nostdb.service
```

The data tree and both credential files remain owned by `nostdb:nostdb`; do not recursively change them back to `root`. The configuration becomes root-owned and group-readable after initialization, while `/etc/nostdb` remains traversable by the `nostdb` group. The unit deliberately omits `ConfigurationDirectory=nostdb` so systemd does not replace this root-managed configuration boundary. `nostd init` is intentionally not an `ExecStartPre` action because it is a one-time, fail-if-existing operation.
