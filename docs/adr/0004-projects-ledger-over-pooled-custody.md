# Projects are a ledger over pooled custody, not separate wallets

One merchant runs several projects on one gateway instance. A project owns its merchant keys, deposit addresses, withdrawals, webhook endpoints and address-book entries, but not separate funds: every deposit address still sweeps into the single treasury of its network, and Lightning stays one Spark wallet. What a project owns is a **project balance** computed from its own rows (confirmed deposits and unattributed funds of its addresses, minus its withdrawals and same-asset fees, plus or minus internal transfers). A withdrawal requested by a project's key may not exceed that balance; admins may override it. The difference between what the gateway really holds and the sum of the project balances is the **merchant funds**, and a sustained negative difference raises a **ledger shortfall** alert.

## Considered Options

- **One instance per project**: real isolation of seeds, databases and Lightning wallets, with no application code, but two deployments, two dashboards, and no internal transfers.
- **Per-project derivation and treasuries on one seed**: funds physically apart on chain, but it needs new derivation paths next to frozen ones (the treasury paths already use account `1'`), and Lightning cannot be split (one wallet per seed).
- **Ledger over pooled custody** (chosen): one custody model, cheap internal transfers, and per-project visibility and limits.

## Consequences

- A compromise of the gateway exposes every project at once: the separation is about accounting and visibility, not custody.
- Sweep and gas costs are not charged to projects; they reduce the merchant funds, which the merchant keeps positive by funding the gas reserves.
- Moving project funds to cold storage is a withdrawal debited from a project, and bringing them back is a treasury deposit followed by an internal transfer back to that project. That keeps "project balance = what its keys can withdraw from the hot wallet" true.
- Project balances are always derived from source rows, never stored as counters, so they cannot drift from the history they summarize.
