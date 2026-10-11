-- Security hardening (SECURITY_AUDIT.md rev 2).
--
-- Unique indexes over pre-existing data are created only when that data
-- holds no duplicate: a failed statement would abort this migration (and,
-- migrations being non-fatal at boot, every later one).

-- #37 — derivation indices are allocated atomically from a per-network
-- counter (no more MAX + 1 race), seeded past every index already issued.
CREATE TABLE "derivation_counters" (
    "network" "Network" NOT NULL,
    "nextIndex" INTEGER NOT NULL,
    CONSTRAINT "derivation_counters_pkey" PRIMARY KEY ("network")
);
INSERT INTO "derivation_counters" ("network", "nextIndex")
SELECT network, MAX("derivationIndex") + 1 FROM deposit_addresses GROUP BY network;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM deposit_addresses
        GROUP BY network, "derivationIndex" HAVING COUNT(*) > 1
    ) THEN
        CREATE UNIQUE INDEX "deposit_addresses_network_derivationIndex_key"
            ON deposit_addresses(network, "derivationIndex");
    ELSE
        RAISE WARNING 'deposit_addresses has duplicate (network, derivationIndex) pairs — unique index skipped';
    END IF;
END $$;

-- #54 — the merchant key that opened a payment (NULL when created from the
-- dashboard), so open payments can be capped per key.
ALTER TABLE deposit_addresses ADD COLUMN "apiKeyId" TEXT;
ALTER TABLE deposit_addresses ADD CONSTRAINT "deposit_addresses_apiKeyId_fkey"
    FOREIGN KEY ("apiKeyId") REFERENCES "api_keys"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
CREATE INDEX "deposit_addresses_apiKeyId_pending_idx"
    ON deposit_addresses("apiKeyId") WHERE status = 'PENDING'::"DepositAddressStatus";

-- #56 — one delivery row per logical key (get-or-create uses
-- ON CONFLICT DO NOTHING, which also works where an index was skipped).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM webhook_deliveries
        WHERE "depositId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL
        GROUP BY "depositId", "webhookEndpointId" HAVING COUNT(*) > 1
    ) THEN
        CREATE UNIQUE INDEX "webhook_deliveries_paid_key"
            ON webhook_deliveries("depositId", "webhookEndpointId")
            WHERE "depositId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL;
    ELSE
        RAISE WARNING 'duplicate PAID webhook deliveries — unique index skipped';
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM webhook_deliveries
        WHERE "depositId" IS NULL AND "depositAddressId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL
        GROUP BY "depositAddressId", "eventType", "webhookEndpointId" HAVING COUNT(*) > 1
    ) THEN
        CREATE UNIQUE INDEX "webhook_deliveries_address_event_key"
            ON webhook_deliveries("depositAddressId", "eventType", "webhookEndpointId")
            WHERE "depositId" IS NULL AND "depositAddressId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL;
    ELSE
        RAISE WARNING 'duplicate address-event webhook deliveries — unique index skipped';
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM webhook_deliveries
        WHERE "lightningPaymentId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL
        GROUP BY "lightningPaymentId", "eventType", "webhookEndpointId" HAVING COUNT(*) > 1
    ) THEN
        CREATE UNIQUE INDEX "webhook_deliveries_payout_key"
            ON webhook_deliveries("lightningPaymentId", "eventType", "webhookEndpointId")
            WHERE "lightningPaymentId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL;
    ELSE
        RAISE WARNING 'duplicate payout webhook deliveries — unique index skipped';
    END IF;
END $$;

-- #40 — a key allowed to send payouts must expire. Keys granted before this
-- rule keep working for 90 more days instead of losing the permission.
UPDATE api_keys
SET "expiresAt" = (NOW() AT TIME ZONE 'UTC') + INTERVAL '90 days'
WHERE "canSend" AND "expiresAt" IS NULL;
ALTER TABLE api_keys ADD CONSTRAINT "api_keys_can_send_expiry_check"
    CHECK (NOT "canSend" OR "expiresAt" IS NOT NULL);

-- #48 — payout requests: idempotency bound to the caller and the
-- parameters, and the ledger the payout limits are checked against.
CREATE TYPE "PayoutRequestStatus" AS ENUM ('PENDING', 'SENT', 'FAILED');
CREATE TABLE "payout_requests" (
    "id" TEXT NOT NULL,
    "idempotencyKey" TEXT NOT NULL,
    "apiKeyId" TEXT,
    "staffId" TEXT,
    -- SHA-256 of the normalized (ln_address, amount_sats, comment).
    "paramsHash" TEXT NOT NULL,
    "lnAddress" TEXT NOT NULL,
    "amountSats" BIGINT NOT NULL,
    "comment" TEXT,
    "status" "PayoutRequestStatus" NOT NULL,
    -- SDK Payment.id once the send is accepted.
    "paymentId" TEXT,
    "lastError" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "payout_requests_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "payout_requests_amount_check" CHECK ("amountSats" > 0),
    CONSTRAINT "payout_requests_caller_check" CHECK (("apiKeyId" IS NULL) <> ("staffId" IS NULL))
);
CREATE UNIQUE INDEX "payout_requests_idempotencyKey_key" ON "payout_requests"("idempotencyKey");
CREATE INDEX "payout_requests_createdAt_idx" ON "payout_requests"("createdAt");
ALTER TABLE "payout_requests" ADD CONSTRAINT "payout_requests_apiKeyId_fkey"
    FOREIGN KEY ("apiKeyId") REFERENCES "api_keys"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "payout_requests" ADD CONSTRAINT "payout_requests_staffId_fkey"
    FOREIGN KEY ("staffId") REFERENCES "staff_users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- #49 — append-only audit trail of security-relevant actions.
CREATE TYPE "AuditActorType" AS ENUM ('STAFF', 'API_KEY', 'SYSTEM');
CREATE TABLE "audit_log" (
    "id" TEXT NOT NULL,
    "actorType" "AuditActorType" NOT NULL,
    "actorId" TEXT,
    "action" TEXT NOT NULL,
    "targetType" TEXT,
    "targetId" TEXT,
    "details" JSONB NOT NULL DEFAULT '{}',
    "ip" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "audit_log_pkey" PRIMARY KEY ("id")
);
CREATE INDEX "audit_log_createdAt_idx" ON "audit_log"("createdAt" DESC);
CREATE INDEX "audit_log_action_createdAt_idx" ON "audit_log"("action", "createdAt" DESC);

CREATE FUNCTION "audit_log_append_only"() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'audit_log is append-only';
END
$$ LANGUAGE plpgsql;
CREATE TRIGGER "audit_log_no_update_delete" BEFORE UPDATE OR DELETE ON "audit_log"
    FOR EACH ROW EXECUTE FUNCTION "audit_log_append_only"();
CREATE TRIGGER "audit_log_no_truncate" BEFORE TRUNCATE ON "audit_log"
    FOR EACH STATEMENT EXECUTE FUNCTION "audit_log_append_only"();

-- #29 — the traffic log (client IPs) is kept 90 days by default instead of
-- 2 years; an explicitly chosen retention is left untouched.
UPDATE app_settings SET "value" = '90', "updatedAt" = NOW()
WHERE "key" = 'apiTrafficRetentionDays' AND "value" = '730';
