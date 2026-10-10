-- Bitcoin (on-chain, BIP84) and Lightning (BOLT11 via Breez SDK) deposits.
--
-- ADD VALUE runs inside sqlx's migration transaction: allowed since
-- Postgres 12 as long as the new values are not used in the same
-- transaction (they are not).
ALTER TYPE "Network" ADD VALUE IF NOT EXISTS 'BITCOIN';
ALTER TYPE "Network" ADD VALUE IF NOT EXISTS 'LIGHTNING';
ALTER TYPE "Token" ADD VALUE IF NOT EXISTS 'BTC';

-- Per-token decimals: BTC has 8 (max 21 000 000.00000000). Existing
-- 6-decimal USDT values stay exact in the wider column.
ALTER TABLE deposit_addresses ALTER COLUMN "expectedAmount" TYPE DECIMAL(24,8);
ALTER TABLE deposits ALTER COLUMN "amount" TYPE DECIMAL(24,8);

-- One Bitcoin transaction can pay several gateway addresses: a deposit is
-- identified by (address, tx), not by the tx alone.
DROP INDEX "deposits_txHash_key";
CREATE UNIQUE INDEX "deposits_addr_txHash_key" ON deposits("depositAddressId", "txHash");
