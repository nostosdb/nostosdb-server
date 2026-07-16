# nostos-server

Source-available server consumer of the NostosDB engine, licensed under SSPL-1.0.

Stage 0 supplies a compiling binary skeleton only. It opens no sockets and implements no HTTP endpoints, sessions, authentication, transactions, or resource limits. All server behavior is deferred to Stage 8.

The local development manifest depends only on the sibling public `nostos-engine` facade, not Core internals.

## Verify

```bash
cargo metadata --no-deps
cargo fmt --all --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## License

Source-available under SSPL-1.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
