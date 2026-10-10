-- API traffic log: one row per HTTP request under /api/v1 (merchant) and
-- /api/admin/v1 (dashboard). No request/response bodies and never the raw
-- x-api-key — only the resolved api_keys.id / staff_users.id.
CREATE TYPE "ApiSurface" AS ENUM ('MERCHANT', 'ADMIN');

-- BIGINT identity instead of a TEXT uuid on purpose: high-volume, append-only,
-- never referenced by other tables.
CREATE TABLE "api_requests" (
    "id"         BIGINT GENERATED ALWAYS AS IDENTITY,
    "surface"    "ApiSurface" NOT NULL,
    "method"     TEXT NOT NULL,
    -- Matched route template (e.g. /api/v1/deposit-addresses/{id}), never the
    -- raw path, so cardinality stays bounded.
    "route"      TEXT NOT NULL,
    "status"     SMALLINT NOT NULL,
    "durationMs" DOUBLE PRECISION NOT NULL,
    -- api_keys.id for authenticated merchant calls; NULL otherwise (including
    -- rejected key attempts, which is what a 401 with no key looks like).
    "apiKeyId"   TEXT,
    -- staff_users.id for authenticated dashboard calls; NULL otherwise.
    "staffId"    TEXT,
    "ip"         TEXT,
    "createdAt"  TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "api_requests_pkey" PRIMARY KEY ("id")
);

CREATE INDEX "api_requests_createdAt_idx" ON "api_requests"("createdAt");
CREATE INDEX "api_requests_surface_createdAt_idx" ON "api_requests"("surface", "createdAt");
CREATE INDEX "api_requests_apiKeyId_createdAt_idx" ON "api_requests"("apiKeyId", "createdAt")
    WHERE "apiKeyId" IS NOT NULL;

-- Retention in days, managed from the dashboard (Settings). '0' = keep forever.
INSERT INTO "app_settings" ("key", "value") VALUES ('apiTrafficRetentionDays', '730');
