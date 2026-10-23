-- A payment is the confirmed total of its transfers, watched after its
-- deadline (docs/adr/0005-un-paiement-est-la-somme-de-ses-envois.md).

-- The end of the late watch: a paid or expired address is still polled
-- until then. Existing rows get the default watch (24 hours); new rows
-- `expiresAt` + LATE_WATCH_HOURS. A timestamp in UTC like "expiresAt".
ALTER TABLE "deposit_addresses" ADD COLUMN "watchUntil" TIMESTAMP(3);
UPDATE "deposit_addresses" SET "watchUntil" = "expiresAt" + interval '24 hours';
ALTER TABLE "deposit_addresses" ALTER COLUMN "watchUntil" SET NOT NULL;
CREATE INDEX "deposit_addresses_network_watchUntil_idx" ON "deposit_addresses"("network", "watchUntil");

-- A late deposit: first seen once its address had left PENDING. It never
-- changes the address's status.
ALTER TABLE "deposits" ADD COLUMN "late" BOOLEAN NOT NULL DEFAULT false;
-- The pollers keep an address while one of its deposits still confirms.
CREATE INDEX "deposits_detected_idx" ON "deposits"("depositAddressId") WHERE "status" = 'DETECTED';

-- Deposit deliveries are keyed on (deposit, event, endpoint): a deposit
-- notifies PAID or LATE. The former key had no event; adding one only
-- relaxes it, but an instance that skipped it (duplicates) skips this one too.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM webhook_deliveries
        WHERE "depositId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL
        GROUP BY "depositId", "eventType", "webhookEndpointId" HAVING COUNT(*) > 1
    ) THEN
        DROP INDEX IF EXISTS "webhook_deliveries_paid_key";
        CREATE UNIQUE INDEX "webhook_deliveries_deposit_event_key"
            ON webhook_deliveries("depositId", "eventType", "webhookEndpointId")
            WHERE "depositId" IS NOT NULL AND "webhookEndpointId" IS NOT NULL;
    ELSE
        RAISE WARNING 'duplicate deposit webhook deliveries — unique index skipped';
    END IF;
END $$;
