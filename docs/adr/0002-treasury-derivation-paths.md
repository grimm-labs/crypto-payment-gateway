# Treasuries are derived from the same seed on reserved paths

Treasury addresses are derived from `WALLET_MNEMONIC` on paths no deposit index can ever reach: Ethereum and Tron `m/44'/60'/1'/0/0` (account 1; the same key for both, as deposits already share keys across the two networks), Bitcoin `m/84'/0'/1'/0/0` (also used for change), Solana `m/44'/501'/0'/1'` (the `1'` change branch that deposits never use). Everything stays recoverable from the single mnemonic, and no convention about "reserved deposit indices" is needed. These paths must never change once funds sit on them — changing them strands the treasury exactly like changing a deposit path would.

## Considered Options

- **A dedicated `TREASURY_MNEMONIC`**: a second hot secret in the same process brings little isolation and doubles backup and deployment work.
- **Reserving a deposit index** (e.g. the highest one): same branch for two roles, protected only by a convention the derivation counter could one day cross.
