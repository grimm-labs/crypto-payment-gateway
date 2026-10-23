-- Deliveries are written PENDING with their status change and queued after
-- the commit; every minute the reconciliation reads the PENDING ones to queue
-- again those whose Redis member was lost (spec §4.5). PENDING rows are few
-- (deliveries waiting for a retry): a partial index keeps that read small.
CREATE INDEX "webhook_deliveries_pending_idx" ON "webhook_deliveries"("id")
    WHERE "status" = 'PENDING' AND "webhookEndpointId" IS NOT NULL;
