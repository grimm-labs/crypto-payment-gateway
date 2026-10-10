-- Lightning-address payouts.

-- Merchant keys may pay only once an admin grants it.
ALTER TABLE api_keys ADD COLUMN "canSend" BOOLEAN NOT NULL DEFAULT false;

-- Payout webhooks are keyed on a Lightning wallet payment id instead of a
-- deposit address: every delivery has exactly one of the two subjects.
ALTER TABLE webhook_deliveries ALTER COLUMN "depositAddressId" DROP NOT NULL;
ALTER TABLE webhook_deliveries ADD COLUMN "lightningPaymentId" TEXT;
ALTER TABLE webhook_deliveries ADD CONSTRAINT "webhook_deliveries_subject_check"
    CHECK (("depositAddressId" IS NULL) <> ("lightningPaymentId" IS NULL));
CREATE INDEX "webhook_deliveries_payout_key_idx"
    ON webhook_deliveries("lightningPaymentId", "eventType", "webhookEndpointId");
