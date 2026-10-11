-- Native ETH deposits, and the gas assets (TRX, SOL) withdrawals can move.
--
-- On its own on purpose: ADD VALUE runs inside sqlx's migration transaction
-- (Postgres ≥ 12) only as long as the new values are not used in that same
-- transaction, and the next migration seeds rows with them.
ALTER TYPE "Token" ADD VALUE IF NOT EXISTS 'ETH';
ALTER TYPE "Token" ADD VALUE IF NOT EXISTS 'TRX';
ALTER TYPE "Token" ADD VALUE IF NOT EXISTS 'SOL';
