-- Projects: several applications on one gateway instance
-- (docs/projects-spec.md §16). Every statement is safe on existing data:
-- new tables, nullable owner columns (NULL = legacy row, or the merchant),
-- partial unique indexes that are looser than the ones they replace, and a
-- CHECK that existing rows already satisfy (every TRX/SOL withdrawal is an
-- admin one with no project).

-- 1. New alert type. ADD VALUE runs inside sqlx's migration transaction
--    (Postgres ≥ 12) as long as the value is not used in it: it is not.
ALTER TYPE "AlertType" ADD VALUE IF NOT EXISTS 'LEDGER_SHORTFALL';

-- 2. Projects (§6.1).
CREATE TABLE "projects" (
    "id" TEXT NOT NULL,
    -- 1–60 characters after trim; unique ignoring case.
    "name" TEXT NOT NULL,
    "color" TEXT NOT NULL,
    -- Tag of the keys generated for the project; NULL = the global tag.
    "keyTag" TEXT,
    "createdById" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "projects_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "projects_name_check" CHECK (char_length("name") BETWEEN 1 AND 60),
    CONSTRAINT "projects_color_check" CHECK (
        "color" IN ('blue', 'emerald', 'amber', 'rose', 'violet', 'cyan', 'orange', 'fuchsia')
    ),
    CONSTRAINT "projects_keyTag_check" CHECK ("keyTag" IS NULL OR "keyTag" ~ '^[A-Za-z0-9]{1,5}$')
);
CREATE UNIQUE INDEX "projects_name_key" ON "projects"(lower("name"));
CREATE UNIQUE INDEX "projects_keyTag_key" ON "projects"(lower("keyTag")) WHERE "keyTag" IS NOT NULL;
ALTER TABLE "projects" ADD CONSTRAINT "projects_createdById_fkey"
    FOREIGN KEY ("createdById") REFERENCES "staff_users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- 3. Owner columns (§6.2). NULL = a legacy row from before the first
--    project (keys, payments) or the merchant (withdrawals, endpoints,
--    address-book entries).
ALTER TABLE "api_keys" ADD COLUMN "projectId" TEXT;
ALTER TABLE "api_keys" ADD CONSTRAINT "api_keys_projectId_fkey"
    FOREIGN KEY ("projectId") REFERENCES "projects"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
CREATE INDEX "api_keys_projectId_idx" ON "api_keys"("projectId");

ALTER TABLE "deposit_addresses" ADD COLUMN "projectId" TEXT;
ALTER TABLE "deposit_addresses" ADD CONSTRAINT "deposit_addresses_projectId_fkey"
    FOREIGN KEY ("projectId") REFERENCES "projects"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
CREATE INDEX "deposit_addresses_projectId_createdAt_idx" ON "deposit_addresses"("projectId", "createdAt" DESC);
CREATE INDEX "deposit_addresses_projectId_status_idx" ON "deposit_addresses"("projectId", status);

ALTER TABLE "withdrawals" ADD COLUMN "projectId" TEXT;
ALTER TABLE "withdrawals" ADD CONSTRAINT "withdrawals_projectId_fkey"
    FOREIGN KEY ("projectId") REFERENCES "projects"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
CREATE INDEX "withdrawals_projectId_createdAt_idx" ON "withdrawals"("projectId", "createdAt" DESC);
CREATE INDEX "withdrawals_projectId_asset_status_idx" ON "withdrawals"("projectId", network, token, status);
-- Gas assets are always merchant funds (projects never hold TRX/SOL).
ALTER TABLE "withdrawals" ADD CONSTRAINT "withdrawals_gas_asset_merchant_check"
    CHECK (token NOT IN ('TRX', 'SOL') OR "projectId" IS NULL);

ALTER TABLE "webhook_endpoints" ADD COLUMN "projectId" TEXT;
ALTER TABLE "webhook_endpoints" ADD CONSTRAINT "webhook_endpoints_projectId_fkey"
    FOREIGN KEY ("projectId") REFERENCES "projects"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

ALTER TABLE "address_book_entries" ADD COLUMN "projectId" TEXT;
ALTER TABLE "address_book_entries" ADD CONSTRAINT "address_book_entries_projectId_fkey"
    FOREIGN KEY ("projectId") REFERENCES "projects"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- 4. Uniqueness per owner (§6.2). Existing rows are already unique under
--    the stricter indexes they replace, so the new ones cannot fail.
DROP INDEX IF EXISTS "webhook_endpoints_url_key";
CREATE UNIQUE INDEX "webhook_endpoints_project_url_key"
    ON "webhook_endpoints"("projectId", url) WHERE "projectId" IS NOT NULL;
CREATE UNIQUE INDEX "webhook_endpoints_merchant_url_key"
    ON "webhook_endpoints"(url) WHERE "projectId" IS NULL;

DROP INDEX IF EXISTS "address_book_entries_network_destination_key";
CREATE UNIQUE INDEX "address_book_entries_project_destination_key"
    ON "address_book_entries"("projectId", network, destination)
    WHERE "projectId" IS NOT NULL AND "deletedAt" IS NULL;
CREATE UNIQUE INDEX "address_book_entries_merchant_destination_key"
    ON "address_book_entries"(network, destination)
    WHERE "projectId" IS NULL AND "deletedAt" IS NULL;

-- 5. Internal transfers (§6.3): an admin's ledger move between two owners
--    (NULL = the merchant funds). Nothing moves on chain. Append-only.
CREATE TABLE "internal_transfers" (
    "id" TEXT NOT NULL,
    "idempotencyKey" TEXT NOT NULL,
    -- SHA-256 of the normalized from, to, network, token, amount and note.
    "paramsHash" TEXT NOT NULL,
    "network" "Network" NOT NULL,
    "token" "Token" NOT NULL,
    "amountRaw" NUMERIC(78,0) NOT NULL,
    "fromProjectId" TEXT,
    "toProjectId" TEXT,
    "note" TEXT NOT NULL,
    "staffId" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "internal_transfers_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "internal_transfers_amount_check" CHECK ("amountRaw" > 0),
    CONSTRAINT "internal_transfers_sides_check" CHECK ("fromProjectId" IS DISTINCT FROM "toProjectId"),
    CONSTRAINT "internal_transfers_note_check" CHECK (char_length("note") BETWEEN 1 AND 200)
);
CREATE UNIQUE INDEX "internal_transfers_idempotencyKey_key" ON "internal_transfers"("idempotencyKey");
CREATE INDEX "internal_transfers_fromProjectId_createdAt_idx" ON "internal_transfers"("fromProjectId", "createdAt" DESC);
CREATE INDEX "internal_transfers_toProjectId_createdAt_idx" ON "internal_transfers"("toProjectId", "createdAt" DESC);
CREATE INDEX "internal_transfers_network_token_idx" ON "internal_transfers"(network, token);
ALTER TABLE "internal_transfers" ADD CONSTRAINT "internal_transfers_fromProjectId_fkey"
    FOREIGN KEY ("fromProjectId") REFERENCES "projects"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "internal_transfers" ADD CONSTRAINT "internal_transfers_toProjectId_fkey"
    FOREIGN KEY ("toProjectId") REFERENCES "projects"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "internal_transfers" ADD CONSTRAINT "internal_transfers_staffId_fkey"
    FOREIGN KEY ("staffId") REFERENCES "staff_users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

CREATE FUNCTION "internal_transfers_append_only"() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'internal_transfers is append-only';
END
$$ LANGUAGE plpgsql;
CREATE TRIGGER "internal_transfers_no_update_delete" BEFORE UPDATE OR DELETE ON "internal_transfers"
    FOR EACH ROW EXECUTE FUNCTION "internal_transfers_append_only"();
CREATE TRIGGER "internal_transfers_no_truncate" BEFORE TRUNCATE ON "internal_transfers"
    FOR EACH STATEMENT EXECUTE FUNCTION "internal_transfers_append_only"();

-- 6. Bitcoin received watermark (§6.4): the total confirmed amount ever
--    received by a Bitcoin deposit address, only ever raised. NULL on every
--    other network.
ALTER TABLE "deposit_addresses" ADD COLUMN "btcReceivedRaw" NUMERIC(78,0);

-- 7. Webhook deliveries about an internal transfer (§6.5): exactly one
--    subject per delivery (existing rows have exactly one of the first four).
ALTER TABLE "webhook_deliveries" ADD COLUMN "transferId" TEXT;
ALTER TABLE "webhook_deliveries" ADD CONSTRAINT "webhook_deliveries_transferId_fkey"
    FOREIGN KEY ("transferId") REFERENCES "internal_transfers"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
ALTER TABLE "webhook_deliveries" DROP CONSTRAINT "webhook_deliveries_subject_check";
ALTER TABLE "webhook_deliveries" ADD CONSTRAINT "webhook_deliveries_subject_check"
    CHECK (num_nonnulls("depositAddressId", "lightningPaymentId", "withdrawalId", "alertId", "transferId") = 1);
-- The column is new (no data): nothing to guard against.
CREATE UNIQUE INDEX "webhook_deliveries_transfer_key"
    ON "webhook_deliveries"("transferId", "eventType", "webhookEndpointId")
    WHERE "transferId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL;

-- 8. No backfill: attaching legacy rows is the application's first-project
--    step (§13).
