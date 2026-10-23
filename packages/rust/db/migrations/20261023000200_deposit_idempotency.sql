-- `Idempotency-Key` on POST /deposit-addresses (optional): a retry with the
-- same key and the same parameters answers the stored payment, other
-- parameters are refused. Scoped to the merchant key that sent it.
ALTER TABLE "deposit_addresses" ADD COLUMN "idempotencyKey" TEXT;
ALTER TABLE "deposit_addresses" ADD COLUMN "paramsHash" TEXT;
CREATE UNIQUE INDEX "deposit_addresses_apiKeyId_idempotencyKey_key"
    ON "deposit_addresses"("apiKeyId", "idempotencyKey")
    WHERE "idempotencyKey" IS NOT NULL;
