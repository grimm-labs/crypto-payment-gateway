# Transactions are signed inside the gateway process, behind a `Signer` interface

Withdrawals, sweeps and gas top-ups are signed by the gateway itself, with keys derived on demand from the seed it already holds (as the Lightning wallet already does), rather than by a separate signer service or a KMS/HSM. This keeps the deployment unchanged (one API role) and needs no new heavy dependency (alloy and rust-bitcoin are already in the lockfile). All signing goes through a `Signer` trait in the `wallet` crate — the only code that ever sees private keys — so moving to an isolated signer process or an external KMS later only means another `Signer` implementation.

## Considered Options

- **Separate signer process holding the seed**: the internet-facing API would no longer hold keys, at the cost of an extra service and an IPC protocol to secure and deploy.
- **KMS / HSM**: strongest custody, but cloud lock-in, uneven secp256k1/ed25519 support, and incompatible with the existing HD derivation without a redesign.

## Consequences

- A compromise of the API process is a compromise of every hot fund: the guard rails (limits, address book, approval, outflow freeze, hot-wallet cap alerts) are what bound the damage.
- Solana signatures are produced with ed25519-dalek 2 and inserted by hand, never through `solana_sdk::Keypair` (ed25519-dalek 1.x, whose advisories `deny.toml` ignores precisely because we never sign with it).
