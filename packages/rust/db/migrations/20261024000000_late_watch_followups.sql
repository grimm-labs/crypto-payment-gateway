-- Follow-ups of the late watch (docs/adr/0005-un-paiement-est-la-somme-de-ses-envois.md).

-- 1. No late watch for payments created before it existed. 20261023000000
-- gave every existing address a 24-hour watch, which would record as late
-- deposits the transfers the former code deliberately ignored. That
-- migration may already be applied (test instances), so it is not edited:
-- the addresses created before it ran (its `installed_on` in sqlx's own
-- table) are watched until their deadline only.
UPDATE "deposit_addresses"
SET "watchUntil" = "expiresAt"
WHERE "createdAt" < (
    SELECT "installed_on" AT TIME ZONE 'UTC' FROM "_sqlx_migrations" WHERE "version" = 20261023000000
);

-- 2. The webhook event a deposit's confirmation fired: PAID (it paid its
-- address), LATE (its address had already left PENDING), NULL (a partial
-- transfer). Manual replays send that event and nothing else. Deposits
-- confirmed before the late watch each fired PAID (the former rule); late
-- ones recorded since fired LATE.
ALTER TABLE "deposits" ADD COLUMN "webhookEvent" TEXT;
UPDATE "deposits" d
SET "webhookEvent" = 'PAID'
WHERE d."status" = 'CONFIRMED' AND NOT d."late" AND d."confirmedAt" < (
    SELECT "installed_on" AT TIME ZONE 'UTC' FROM "_sqlx_migrations" WHERE "version" = 20261023000000
);
UPDATE "deposits" SET "webhookEvent" = 'LATE' WHERE "status" = 'CONFIRMED' AND "late";

-- 3. The confirmed total of the address when a PAID / LATE / EXPIRED
-- delivery row is written: every attempt of the delivery (one `event_id`)
-- sends the same `received_amount_raw`.
ALTER TABLE "webhook_deliveries" ADD COLUMN "receivedRaw" NUMERIC(78,0);

-- 4. `is_gateway_outbound_tx` matches hashes case-insensitively (Ethereum
-- hashes come in two casings) on every newly seen transfer.
CREATE INDEX "outbound_transactions_network_lower_txHash_idx"
    ON "outbound_transactions"("network", lower("txHash"));
