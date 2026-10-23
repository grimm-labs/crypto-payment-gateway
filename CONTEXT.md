# Crypto Payment Gateway

A custodial gateway that receives crypto payments for one merchant's projects on several networks, notifies the merchant through webhooks, and sends funds back out on request.

## Language

### Receiving

**Deposit address**:
A one-off payment request: a fresh address (or a Lightning invoice) issued for one expected amount of one asset, which expires if unpaid.
_Avoid_: Payment address, invoice (except for Lightning)

**Deposit**:
An incoming on-chain or Lightning transfer credited to a deposit address, whatever its amount. A deposit address is paid when the confirmed total of its deposits reaches its expected amount, less the tolerance; more pays too.
_Avoid_: Payment, incoming transaction

**Late deposit**:
A deposit first seen once its address was already paid or expired, during the late watch that follows the deadline; it is the project's money but never changes the address's status.
_Avoid_: Overpayment, orphan deposit

**Late watch**:
The hours after a deposit address's deadline during which it is still watched for late deposits.
_Avoid_: Grace period (that is the 10 minutes before an address expires)

**Unattributed funds**:
Value sitting on a deposit address that the gateway never recorded as a deposit (arrived after the late watch, wrong asset); it is swept like the rest but flagged.
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

### Projects

**Project balance**:
What a project has brought into the gateway and not taken out, per asset and network: its confirmed deposits and the unattributed funds of its deposit addresses, minus its withdrawals and their fees paid in the same asset, plus or minus its internal transfers. It is a ledger figure; the funds themselves are pooled in the treasuries.
_Avoid_: Project wallet, sub-account, project funds

**Merchant funds**:
What the gateway holds beyond the sum of the project balances, per asset and network: mostly the gas reserves the merchant funds, less the costs no project is charged for (sweeps, fees paid in a gas asset). A withdrawal that names no project draws on them.
_Avoid_: Instance funds, operator share, unassigned funds

**Internal transfer**:
An admin's ledger move of an amount of one asset on one network from one owner to another; nothing moves on chain.
_Avoid_: Withdrawal, internal withdrawal, reallocation

**Owner**:
The project a withdrawal, internal transfer side, webhook endpoint or address-book entry belongs to, or the merchant when it belongs to no project.
_Avoid_: Scope, tenant

**Ledger shortfall**:
The state in which the project balances of an asset on a network add up to more than the gateway actually holds there, i.e. negative merchant funds.
_Avoid_: Deficit, negative gap

### Actors

**Merchant**:
The operator of the gateway: the single owner of its funds and of every project.
_Avoid_: Tenant, client, customer

**Project**:
One of the merchant's applications that takes payments through the gateway.
_Avoid_: Tenant, workspace, account, merchant

**Merchant key**:
An API key through which a project's systems call the gateway.
_Avoid_: API client, token

**Staff**:
A person operating the gateway from the dashboard, with the role admin or operator.
_Avoid_: User, account
