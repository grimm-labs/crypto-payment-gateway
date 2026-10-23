# A payment is the confirmed total of its transfers, watched 24 hours after its deadline

A deposit address is paid when the **confirmed total** of every transfer to it reaches its expected amount, less the tolerance (`AMOUNT_TOLERANCE_PERCENT`, 1 %): `total × 100 ≥ expected × (100 − tolerance)`, in base units with integer arithmetic. There is no upper bound: an overpayment pays, and what it brings is reported (`received_amount`). Every non-zero transfer is recorded as a deposit whatever its amount, and the address turns `PAID` in the transaction that confirms the deposit making the total reach the amount, under a lock of the address row and only from `PENDING`, so the `paid` webhook fires exactly once. A paid or expired address stays watched for `LATE_WATCH_HOURS` (24) after its deadline: money arriving then is a **late deposit**, the project's money like any deposit, reported by its own `late` webhook (`deposit_late`), and it **never changes the address's status** — the receiver decides what an expired payment completed late means. Lightning is unchanged: one fixed-amount invoice, paid once, never watched late.

Payers send from exchanges that take their fee off the amount, wallets that round, and sometimes in two goes; judging each transfer alone against ±1 % lost those payments and left the money on the address as unattributed funds, and an address was no longer looked at once paid or expired.

## Considered Options

- **Each transfer judged alone** (the former rule): simple, but a payment sent in two parts never pays, an overpayment beyond 1 % is ignored, and the funds end as unattributed.
- **A total within ±1 %**: refuses overpayments, which the payer cannot fix, for no gain: the merchant keeps the surplus and can refund it by hand.
- **Late money changes the address** (an expired address turning paid): would rewrite a status the merchant already acted on (an order failed and paid again); a separate event and a flag on the deposit let the receiver decide.
- **Watch forever**: one provider call per address and polling cycle; 24 hours covers slow confirmations, a payer coming back the next morning and an exchange's withdrawal queue.

## Consequences

- The ledger counts every recorded deposit, late ones included: what used to become unattributed funds at the sweep is now credited to the project. Only transfers seen after the watch, or of another token, remain unattributed.
- Polling grows with the watched set: the open payments plus those of the last 24 hours past their deadline (open payments first). With the 300 ms pause per address, a 10-second cycle carries about 33 addresses per network; beyond that cycles run back to back and detection slows.
- A receiver must read `received_amount_raw` and the status rather than count `deposit_confirmed` webhooks: `paid` fires once per payment, not once per transfer, and a partial transfer fires nothing.
- A gateway gas top-up reaching a watched deposit address (a sweep of another token) is the gateway's own money and is never recorded as a deposit.
- Adding up every transfer makes funds already on the address count too (a restored derivation counter, a reused seed, unswept Bitcoin): a transfer whose block is more than 10 minutes older than the payment's creation is never one of its deposits; an unconfirmed one always is.
