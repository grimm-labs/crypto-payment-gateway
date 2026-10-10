# Contributing

Thanks for your interest in improving this project.

## Prerequisites

- Rust stable toolchain
- `protoc` (the Breez SDK compiles gRPC protos): `brew install protobuf` / `apt-get install protobuf-compiler libprotobuf-dev`
- Docker (for Postgres and Redis via `docker compose`)

## Setup

```bash
cp .env.example .env
# Fill provider API keys if you need on-chain monitoring

docker compose up -d postgres redis
cargo run -p gateway
```

Never commit a real `.env`. Keep secrets and production mnemonics out of the repository.

## Tests

```bash
cargo test -p domain -p wallet -p config -p chain -p lightning-client -p jobs -p auth -p api
```

This matches what CI runs. Prefer small, focused PRs with tests when you change `domain`, `wallet`, `config`, `chain`, `lightning-client`, or `jobs` logic.

## Pull requests

- Keep changes scoped to one concern when possible
- Do not add real mnemonics, API keys, or production credentials
- Describe the why briefly in the PR body
