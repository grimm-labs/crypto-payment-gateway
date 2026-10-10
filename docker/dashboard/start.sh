#!/bin/sh
# Dashboard entrypoint: render the server block from the environment, then run
# nginx in the foreground. API_URL is where /api is proxied (http://api:3000 in
# compose, https://<API_HOST> under Kamal).
set -eu
: "${API_URL:?API_URL must point at the API (e.g. http://api:3000)}"
export PORT="${PORT:-8080}"

mkdir -p /tmp/nginx/conf.d
envsubst '${PORT} ${API_URL}' < /app/dashboard/nginx.conf.template > /tmp/nginx/conf.d/default.conf
exec nginx -e /dev/stderr -c /app/dashboard/nginx.conf -g 'daemon off;'
