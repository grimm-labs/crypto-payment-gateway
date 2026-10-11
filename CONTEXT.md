# Crypto Payment Gateway

A custodial gateway that receives crypto payments for a merchant on several networks, notifies the merchant through webhooks, and sends funds back out on request.

## Language

### Receiving

**Deposit address**:
A one-off payment request: a fresh address (or a Lightning invoice) issued for one expected amount of one asset, which expires if unpaid.
_Avoid_: Payment address, invoice (except for Lightning)

**Deposit**:
An incoming on-chain or Lightning transfer credited to a deposit address.
_Avoid_: Payment, incoming transaction

**Unattributed funds**:
Value sitting on a deposit address that settled no payment (outside tolerance, arrived after expiry, wrong asset); it is swept like the rest but flagged.
_Avoid_: Orphan funds, lost funds

### Sending

**Withdrawal**:
Any transfer of funds out of the gateway to an external destination, whatever the asset or network (Lightning included). On-chain withdrawals leave from the treasury.
_Avoid_: Payout, send, transfer out

**Withdrawal limit**:
The maximum amount of one asset that may leave the gateway in a single withdrawal, and over a rolling 24 hours.
_Avoid_: Cap, quota

**Address book**:
The admin-curated list of external destinations withdrawals may be sent to; a new entry only becomes usable after a cooling-off delay.
_Avoid_: Whitelist, allowlist, beneficiaries

**Destination policy**:
What a merchant key may withdraw to: only address-book entries (book mode), or any destination within the key's own limits (open mode).
_Avoid_: Withdrawal mode, whitelist mode

**Approval**:
The sign-off a second admin must give before a withdrawal above the approval threshold is executed.
_Avoid_: Validation, confirmation (confirmation means block confirmations)

### Custody

**Treasury**:
The single gateway-controlled address per network that consolidated funds sit on and on-chain withdrawals leave from.
_Avoid_: Hot wallet (ambiguous: every derived address is hot), main address

**Sweep**:
Moving the confirmed funds of a deposit address into the treasury.
_Avoid_: Consolidation, collection

**Gas asset**:
A network's native coin (ETH, TRX, SOL) held only to pay network fees; it can be withdrawn but is never accepted as a payment.
_Avoid_: Fee token

**Gas reserve**:
The gas-asset balance of a treasury, funded by the operator, from which fees and gas top-ups are paid.
_Avoid_: Fee wallet, gas tank

**Outflow freeze**:
The emergency state in which no withdrawal or sweep leaves the gateway until an admin lifts it.
_Avoid_: Kill switch, pause

**Gas top-up**:
Sending just enough gas asset from the treasury to a deposit address so its tokens can be swept.
_Avoid_: Funding, refuel

### Actors

**Merchant key**:
An API key through which the merchant's own systems call the gateway.
_Avoid_: API client, token

**Staff**:
A person operating the gateway from the dashboard, with the role admin or operator.
_Avoid_: User, account
