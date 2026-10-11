-- Native ETH deposits and withdrawals for every asset (docs/eth-and-withdrawals-spec.md).
--
-- Amounts of the new tables are integers in base units (NUMERIC(78,0));
-- their decimal rendering is derived in code. Nothing here can fail on
-- existing data: every new constraint is either on a new column/table or
-- already guaranteed by an older one (see each step).

-- 1. Enums (the Token values come from 20261013000000; WebhookEventType is
--    TEXT, its new values need no DDL).
CREATE TYPE "WithdrawalStatus" AS ENUM (
    'PENDING_APPROVAL', 'QUEUED', 'SIGNED', 'BROADCAST', 'CONFIRMED', 'FAILED', 'CANCELLED', 'STUCK'
);
CREATE TYPE "DestinationKind" AS ENUM ('ADDRESS', 'LN_ADDRESS', 'BOLT11');
CREATE TYPE "OutboundPurpose" AS ENUM ('WITHDRAWAL', 'SWEEP', 'GAS_TOPUP');
CREATE TYPE "OutboundTxStatus" AS ENUM ('SIGNED', 'BROADCAST', 'CONFIRMED', 'FAILED', 'REPLACED', 'EXPIRED');
CREATE TYPE "SweepStatus" AS ENUM ('PLANNED', 'TOPUP_SENT', 'READY', 'BROADCAST', 'CONFIRMED', 'FAILED', 'SKIPPED');
CREATE TYPE "WithdrawalPolicy" AS ENUM ('NONE', 'BOOK', 'OPEN');
CREATE TYPE "AlertType" AS ENUM (
    'GAS_RESERVE_LOW', 'TREASURY_CAP_EXCEEDED', 'ADDRESS_BOOK_ENTRY_ADDED', 'WITHDRAWAL_STUCK'
);

-- 2. Wei need 18 decimals. Existing 8-decimal values stay exact.
ALTER TABLE deposit_addresses ALTER COLUMN "expectedAmount" TYPE DECIMAL(38,18);
ALTER TABLE deposits ALTER COLUMN "amount" TYPE DECIMAL(38,18);

-- 3. Tables.

-- Admin-curated destinations; usable once "activeAt" (creation + 24 h) has
-- passed. No update: delete and re-create (restarts the cooling-off).
CREATE TABLE "address_book_entries" (
    "id" TEXT NOT NULL,
    "network" "Network" NOT NULL,
    "destinationKind" "DestinationKind" NOT NULL,
    -- Normalized (EIP-55, lowercase Lightning address…).
    "destination" TEXT NOT NULL,
    "label" TEXT NOT NULL,
    "isColdStorage" BOOLEAN NOT NULL DEFAULT false,
    "createdById" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "activeAt" TIMESTAMP(3) NOT NULL,
    "deletedAt" TIMESTAMP(3),
    "deletedById" TEXT,
    CONSTRAINT "address_book_entries_pkey" PRIMARY KEY ("id"),
    -- BOLT11 invoices are single-use: never book entries.
    CONSTRAINT "address_book_entries_kind_check" CHECK ("destinationKind" IN ('ADDRESS', 'LN_ADDRESS'))
);
CREATE UNIQUE INDEX "address_book_entries_network_destination_key"
    ON "address_book_entries"("network", "destination") WHERE "deletedAt" IS NULL;
ALTER TABLE "address_book_entries" ADD CONSTRAINT "address_book_entries_createdById_fkey"
    FOREIGN KEY ("createdById") REFERENCES "staff_users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "address_book_entries" ADD CONSTRAINT "address_book_entries_deletedById_fkey"
    FOREIGN KEY ("deletedById") REFERENCES "staff_users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- Every transfer out of the gateway, whatever the asset (Lightning included).
CREATE TABLE "withdrawals" (
    "id" TEXT NOT NULL,
    "idempotencyKey" TEXT NOT NULL,
    "apiKeyId" TEXT,
    "staffId" TEXT,
    -- SHA-256 of the normalized (network, token, destination, amount, comment).
    "paramsHash" TEXT NOT NULL,
    "network" "Network" NOT NULL,
    "token" "Token" NOT NULL,
    "destinationKind" "DestinationKind" NOT NULL,
    "destination" TEXT NOT NULL,
    "addressBookEntryId" TEXT,
    -- Solana USDT: the token account resolved at admission (the destination
    -- itself, or the wallet's associated account) — signing never guesses
    -- again whether the destination is a wallet.
    "solanaTokenAccount" TEXT,
    "amountRaw" NUMERIC(78,0) NOT NULL,
    -- Fees, in "feeToken": the quote at creation, the cap the engine may
    -- spend (3 × quote, doubled by an admin bump) and what was actually paid.
    "feeEstimateRaw" NUMERIC(78,0),
    "feeCapRaw" NUMERIC(78,0),
    "feeRaw" NUMERIC(78,0),
    "feeToken" "Token" NOT NULL,
    "status" "WithdrawalStatus" NOT NULL,
    "approvalRequired" BOOLEAN NOT NULL DEFAULT false,
    "approvedById" TEXT,
    "approvedAt" TIMESTAMP(3),
    -- Single active admin at creation: the requester may approve after this.
    "selfApprovalNotBefore" TIMESTAMP(3),
    "approvalExpiresAt" TIMESTAMP(3),
    -- Staff id or API key id of whoever cancelled/rejected (NULL = expiry).
    "cancelledById" TEXT,
    "cancelReason" TEXT,
    -- Current/final tx hash (Lightning: SDK payment id).
    "txHash" TEXT,
    "confirmations" INTEGER NOT NULL DEFAULT 0,
    -- Client-safe code (insufficient_funds, reverted, rejected_by_network…).
    "failureReason" TEXT,
    "reference" TEXT,
    "comment" TEXT,
    "metadata" JSONB NOT NULL DEFAULT '{}',
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    "broadcastAt" TIMESTAMP(3),
    "confirmedAt" TIMESTAMP(3),
    CONSTRAINT "withdrawals_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "withdrawals_amount_check" CHECK ("amountRaw" > 0),
    CONSTRAINT "withdrawals_caller_check" CHECK (("apiKeyId" IS NULL) <> ("staffId" IS NULL)),
    CONSTRAINT "withdrawals_cancel_reason_check"
        CHECK ("cancelReason" IS NULL OR "cancelReason" IN ('rejected', 'expired', 'cancelled'))
);
CREATE UNIQUE INDEX "withdrawals_idempotencyKey_key" ON "withdrawals"("idempotencyKey");
CREATE INDEX "withdrawals_status_idx" ON "withdrawals"("status");
CREATE INDEX "withdrawals_apiKeyId_createdAt_idx" ON "withdrawals"("apiKeyId", "createdAt" DESC);
CREATE INDEX "withdrawals_token_createdAt_idx" ON "withdrawals"("token", "createdAt");
ALTER TABLE "withdrawals" ADD CONSTRAINT "withdrawals_apiKeyId_fkey"
    FOREIGN KEY ("apiKeyId") REFERENCES "api_keys"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "withdrawals" ADD CONSTRAINT "withdrawals_staffId_fkey"
    FOREIGN KEY ("staffId") REFERENCES "staff_users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "withdrawals" ADD CONSTRAINT "withdrawals_approvedById_fkey"
    FOREIGN KEY ("approvedById") REFERENCES "staff_users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "withdrawals" ADD CONSTRAINT "withdrawals_addressBookEntryId_fkey"
    FOREIGN KEY ("addressBookEntryId") REFERENCES "address_book_entries"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- One deposit address swept into its treasury (one token).
CREATE TABLE "sweeps" (
    "id" TEXT NOT NULL,
    "network" "Network" NOT NULL,
    "token" "Token" NOT NULL,
    "depositAddressId" TEXT NOT NULL,
    "fromAddress" TEXT NOT NULL,
    "amountRaw" NUMERIC(78,0) NOT NULL,
    -- Confirmed deposits of the address since its last sweep; the rest of
    -- the amount settled no payment (unattributed funds).
    "creditedRaw" NUMERIC(78,0) NOT NULL DEFAULT 0,
    "unattributedRaw" NUMERIC(78,0) NOT NULL DEFAULT 0,
    -- Gateway cost: the sweep fee plus its gas top-up, in the gas token.
    "feeRaw" NUMERIC(78,0),
    "status" "SweepStatus" NOT NULL,
    "lastError" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "sweeps_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "sweeps_amounts_check" CHECK ("amountRaw" > 0 AND "unattributedRaw" >= 0 AND "creditedRaw" >= 0)
);
CREATE INDEX "sweeps_network_status_idx" ON "sweeps"("network", "status");
CREATE INDEX "sweeps_depositAddressId_createdAt_idx" ON "sweeps"("depositAddressId", "createdAt" DESC);
-- At most one sweep in progress per address, whatever its token: an ETH
-- sweep must never send away the top-up of a USDT sweep in progress.
CREATE UNIQUE INDEX "sweeps_open_key" ON "sweeps"("depositAddressId")
    WHERE "status" NOT IN ('CONFIRMED', 'FAILED', 'SKIPPED');
ALTER TABLE "sweeps" ADD CONSTRAINT "sweeps_depositAddressId_fkey"
    FOREIGN KEY ("depositAddressId") REFERENCES "deposit_addresses"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- Every transaction the gateway signs, stored before it is broadcast; a
-- restart re-broadcasts "signedTx" as is.
CREATE TABLE "outbound_transactions" (
    "id" TEXT NOT NULL,
    "purpose" "OutboundPurpose" NOT NULL,
    "withdrawalId" TEXT,
    -- Sweeps and their gas top-ups.
    "sweepId" TEXT,
    "network" "Network" NOT NULL,
    "token" "Token" NOT NULL,
    "fromAddress" TEXT NOT NULL,
    "toAddress" TEXT NOT NULL,
    "amountRaw" NUMERIC(78,0) NOT NULL,
    -- Ethereum only.
    "nonce" BIGINT,
    "txHash" TEXT NOT NULL,
    -- Hex (Ethereum, Bitcoin, Tron protobuf) or base64 (Solana).
    "signedTx" TEXT NOT NULL,
    -- maxFee/priority/gasLimit, sat/vB, fee_limit, CU price…
    "feeParams" JSONB NOT NULL DEFAULT '{}',
    "feeRaw" NUMERIC(78,0),
    "status" "OutboundTxStatus" NOT NULL,
    -- Fee-bump chain.
    "replacesId" TEXT,
    -- Tron TAPOS / Solana blockhash validity end.
    "expiresAt" TIMESTAMP(3),
    "blockNumber" BIGINT,
    "confirmations" INTEGER NOT NULL DEFAULT 0,
    "lastError" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    "broadcastAt" TIMESTAMP(3),
    "confirmedAt" TIMESTAMP(3),
    CONSTRAINT "outbound_transactions_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "outbound_transactions_subject_check" CHECK (("withdrawalId" IS NULL) <> ("sweepId" IS NULL)),
    CONSTRAINT "outbound_transactions_purpose_check" CHECK (
        ("purpose" = 'WITHDRAWAL') = ("withdrawalId" IS NOT NULL)
    )
);
CREATE UNIQUE INDEX "outbound_transactions_network_txHash_key" ON "outbound_transactions"("network", "txHash");
CREATE INDEX "outbound_transactions_status_network_idx" ON "outbound_transactions"("status", "network");
CREATE INDEX "outbound_transactions_withdrawalId_idx" ON "outbound_transactions"("withdrawalId");
CREATE INDEX "outbound_transactions_sweepId_idx" ON "outbound_transactions"("sweepId");
ALTER TABLE "outbound_transactions" ADD CONSTRAINT "outbound_transactions_withdrawalId_fkey"
    FOREIGN KEY ("withdrawalId") REFERENCES "withdrawals"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "outbound_transactions" ADD CONSTRAINT "outbound_transactions_sweepId_fkey"
    FOREIGN KEY ("sweepId") REFERENCES "sweeps"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "outbound_transactions" ADD CONSTRAINT "outbound_transactions_replacesId_fkey"
    FOREIGN KEY ("replacesId") REFERENCES "outbound_transactions"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- UTXOs spent by an unconfirmed gateway transaction (RBF moves them to the
-- replacement).
CREATE TABLE "btc_utxo_reservations" (
    "txid" TEXT NOT NULL,
    "vout" INTEGER NOT NULL,
    "outboundTxId" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "btc_utxo_reservations_pkey" PRIMARY KEY ("txid", "vout")
);
CREATE INDEX "btc_utxo_reservations_outboundTxId_idx" ON "btc_utxo_reservations"("outboundTxId");
ALTER TABLE "btc_utxo_reservations" ADD CONSTRAINT "btc_utxo_reservations_outboundTxId_fkey"
    FOREIGN KEY ("outboundTxId") REFERENCES "outbound_transactions"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- Ethereum nonce allocation per sending address.
CREATE TABLE "signer_nonces" (
    "network" "Network" NOT NULL,
    "address" TEXT NOT NULL,
    "nextNonce" BIGINT NOT NULL,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "signer_nonces_pkey" PRIMARY KEY ("network", "address")
);

-- Operational alerts, deduplicated on "dedupKey" for 24 h: conditions
-- (gas reserve low, cap exceeded) key on type/network/token — one alert per
-- day while the condition holds; events (address-book entry added,
-- withdrawal stuck) key on their subject, so each one is reported.
CREATE TABLE "alerts" (
    "id" TEXT NOT NULL,
    "type" "AlertType" NOT NULL,
    "network" "Network",
    "token" "Token",
    "dedupKey" TEXT NOT NULL,
    "details" JSONB NOT NULL DEFAULT '{}',
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "alerts_pkey" PRIMARY KEY ("id")
);
CREATE INDEX "alerts_dedupKey_createdAt_idx" ON "alerts"("dedupKey", "createdAt" DESC);
CREATE INDEX "alerts_createdAt_idx" ON "alerts"("createdAt" DESC);

-- Per-asset guard rails (admin + TOTP). NULL cap = none; the gas floor and
-- alert threshold only apply to gas assets.
CREATE TABLE "asset_settings" (
    "token" "Token" NOT NULL,
    "maxPerWithdrawalRaw" NUMERIC(78,0) NOT NULL,
    "dailyMaxRaw" NUMERIC(78,0) NOT NULL,
    "approvalThresholdRaw" NUMERIC(78,0) NOT NULL,
    "hotWalletCapRaw" NUMERIC(78,0),
    "gasReserveFloorRaw" NUMERIC(78,0),
    "gasAlertBelowRaw" NUMERIC(78,0),
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "asset_settings_pkey" PRIMARY KEY ("token")
);

-- Sweep minimum per (network, token).
CREATE TABLE "sweep_settings" (
    "network" "Network" NOT NULL,
    "token" "Token" NOT NULL,
    "minimumRaw" NUMERIC(78,0) NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "sweep_settings_pkey" PRIMARY KEY ("network", "token")
);

-- 4. Merchant keys: destination policy instead of the Lightning-only
--    "canSend" flag.
ALTER TABLE api_keys ADD COLUMN "withdrawalPolicy" "WithdrawalPolicy" NOT NULL DEFAULT 'NONE';
-- DB token values (USDT, BTC, ETH).
ALTER TABLE api_keys ADD COLUMN "withdrawalTokens" TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[];
-- DB network values; NULL = every network of the allowed tokens.
ALTER TABLE api_keys ADD COLUMN "withdrawalNetworks" TEXT[];

CREATE TABLE "api_key_withdrawal_limits" (
    "apiKeyId" TEXT NOT NULL,
    "token" "Token" NOT NULL,
    "maxPerWithdrawalRaw" NUMERIC(78,0) NOT NULL,
    "dailyMaxRaw" NUMERIC(78,0) NOT NULL,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "api_key_withdrawal_limits_pkey" PRIMARY KEY ("apiKeyId", "token")
);
ALTER TABLE "api_key_withdrawal_limits" ADD CONSTRAINT "api_key_withdrawal_limits_apiKeyId_fkey"
    FOREIGN KEY ("apiKeyId") REFERENCES "api_keys"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- Keys that could send Lightning payouts keep exactly that scope: BTC on
-- Lightning only, within the payout limits in force (an admin widens
-- "withdrawalNetworks" for on-chain BTC).
UPDATE api_keys
SET "withdrawalPolicy" = 'OPEN', "withdrawalTokens" = ARRAY['BTC'], "withdrawalNetworks" = ARRAY['LIGHTNING']
WHERE "canSend";

INSERT INTO "api_key_withdrawal_limits" ("apiKeyId", "token", "maxPerWithdrawalRaw", "dailyMaxRaw", "updatedAt")
SELECT k.id, 'BTC',
       COALESCE((SELECT "value"::numeric FROM app_settings WHERE "key" = 'payoutMaxSats' AND "value" ~ '^[0-9]{1,19}$'), 1000000),
       COALESCE((SELECT "value"::numeric FROM app_settings WHERE "key" = 'payoutDailyMaxSats' AND "value" ~ '^[0-9]{1,19}$'), 10000000),
       NOW()
FROM api_keys k
WHERE k."canSend";

-- Held for every key: "canSend" keys already had to expire (previous CHECK).
ALTER TABLE api_keys DROP CONSTRAINT IF EXISTS "api_keys_can_send_expiry_check";
ALTER TABLE api_keys ADD CONSTRAINT "api_keys_withdrawal_expiry_check"
    CHECK ("withdrawalPolicy" = 'NONE' OR "expiresAt" IS NOT NULL);
ALTER TABLE api_keys DROP COLUMN "canSend";

-- 5. Default guard rails (spec §7.16), in base units. BTC carries the
--    payout limits explicitly set before this release.
INSERT INTO "asset_settings"
    ("token", "maxPerWithdrawalRaw", "dailyMaxRaw", "approvalThresholdRaw", "hotWalletCapRaw", "gasReserveFloorRaw", "gasAlertBelowRaw", "updatedAt")
VALUES
    -- 10 000 / 50 000 / 5 000 / cap 100 000 USDT
    ('USDT', 10000000000, 50000000000, 5000000000, 100000000000, NULL, NULL, NOW()),
    -- 5 / 20 / 2 / cap 50 ETH; gas floor 0.05, alert below 0.1 ETH
    ('ETH', 5000000000000000000, 20000000000000000000, 2000000000000000000, 50000000000000000000,
        50000000000000000, 100000000000000000, NOW()),
    -- 10 000 / 50 000 / 5 000 TRX; gas floor 50, alert below 200 TRX
    ('TRX', 10000000000, 50000000000, 5000000000, NULL, 50000000, 200000000, NOW()),
    -- 50 / 200 / 20 SOL; gas floor 0.1, alert below 0.5 SOL
    ('SOL', 50000000000, 200000000000, 20000000000, NULL, 100000000, 500000000, NOW());

INSERT INTO "asset_settings"
    ("token", "maxPerWithdrawalRaw", "dailyMaxRaw", "approvalThresholdRaw", "hotWalletCapRaw", "gasReserveFloorRaw", "gasAlertBelowRaw", "updatedAt")
SELECT 'BTC',
       -- 0.01 BTC (the former payout default) / 0.1 BTC / 0.005 BTC / cap 1 BTC
       COALESCE((SELECT "value"::numeric FROM app_settings WHERE "key" = 'payoutMaxSats' AND "value" ~ '^[0-9]{1,19}$'), 1000000),
       COALESCE((SELECT "value"::numeric FROM app_settings WHERE "key" = 'payoutDailyMaxSats' AND "value" ~ '^[0-9]{1,19}$'), 10000000),
       500000, 100000000, NULL, NULL, NOW();

DELETE FROM app_settings WHERE "key" IN ('payoutMaxSats', 'payoutDailyMaxSats');

-- Sweep minimums: ETH 0.01 · USDT-ERC20 50 · USDT-TRC20 20 · USDT-SPL 1.
INSERT INTO "sweep_settings" ("network", "token", "minimumRaw", "enabled", "updatedAt") VALUES
    ('ETHEREUM', 'ETH', 10000000000000000, true, NOW()),
    ('ETHEREUM', 'USDT', 50000000, true, NOW()),
    ('TRON', 'USDT', 20000000, true, NOW()),
    ('SOLANA', 'USDT', 1000000, true, NOW());

-- 6. Webhook deliveries about a withdrawal or an alert: every delivery has
--    exactly one subject (existing rows have exactly one of the first two).
ALTER TABLE webhook_deliveries ADD COLUMN "withdrawalId" TEXT;
ALTER TABLE webhook_deliveries ADD COLUMN "alertId" TEXT;
ALTER TABLE webhook_deliveries ADD CONSTRAINT "webhook_deliveries_withdrawalId_fkey"
    FOREIGN KEY ("withdrawalId") REFERENCES "withdrawals"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE webhook_deliveries ADD CONSTRAINT "webhook_deliveries_alertId_fkey"
    FOREIGN KEY ("alertId") REFERENCES "alerts"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE webhook_deliveries DROP CONSTRAINT "webhook_deliveries_subject_check";
ALTER TABLE webhook_deliveries ADD CONSTRAINT "webhook_deliveries_subject_check"
    CHECK (num_nonnulls("depositAddressId", "lightningPaymentId", "withdrawalId", "alertId") = 1);

-- One delivery row per logical key. The columns are new (no data), so the
-- duplicate guard of 20261012000000 has nothing to protect against here.
CREATE UNIQUE INDEX "webhook_deliveries_withdrawal_key"
    ON webhook_deliveries("withdrawalId", "eventType", "webhookEndpointId")
    WHERE "withdrawalId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL;
CREATE UNIQUE INDEX "webhook_deliveries_alert_key"
    ON webhook_deliveries("alertId", "eventType", "webhookEndpointId")
    WHERE "alertId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL;
