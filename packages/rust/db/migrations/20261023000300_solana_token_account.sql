-- Solana pays a wallet address (spec §4.4): new Solana deposit addresses are
-- the owner's public key, and this column holds their USDT associated token
-- account, which the pollers and the sweeps read. Existing rows keep their
-- token account in "address" and a NULL here.
ALTER TABLE "deposit_addresses" ADD COLUMN "tokenAccount" TEXT;
CREATE UNIQUE INDEX "deposit_addresses_tokenAccount_key"
    ON "deposit_addresses"("tokenAccount")
    WHERE "tokenAccount" IS NOT NULL;
