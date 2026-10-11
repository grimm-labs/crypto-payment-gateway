# Withdrawals leave from one treasury per network, fed by automatic sweeps

Every payment gets its own derived address, so funds end up scattered over hundreds of addresses. To withdraw any amount reliably we consolidate: confirmed deposits are swept automatically (above a per-asset minimum) into a single treasury address per network, and on-chain withdrawals only ever leave from that treasury. Token sweeps need gas on the deposit address, so each treasury keeps a manually funded gas reserve (ETH, TRX, SOL) and the gateway sends exact gas top-ups; Solana uses the treasury as fee payer instead. Bitcoin is the exception: withdrawals spend deposit UTXOs directly in one multi-input transaction, which is an implicit sweep without the extra fees. Lightning already has a single balance (the Spark wallet).

## Considered Options

- **Withdraw directly from deposit addresses**: no sweep fees, but on account-based chains (Ethereum, Tron, Solana) a withdrawal larger than any single address needs one transaction per address, each needing its own gas — unworkable for large amounts.
- **Sweep at withdrawal time only**: fewer sweeps, but a withdrawal would wait for top-ups and confirmations and fail whenever a sweep fails.

## Consequences

- Sweeps and gas top-ups are a running cost borne by the gateway, and the operator must keep the gas reserves funded (alerts fire below a threshold).
- Sweep destinations are computed from the seed, never read from storage, so tampering with the database cannot redirect sweeps.
- Funds that settled no payment (unattributed funds) are swept too, and traced per source address.
