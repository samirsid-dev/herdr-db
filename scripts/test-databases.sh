#!/bin/sh
# Starts PostgreSQL and MySQL for the integration tests and prints the
# variables to export. Requires Docker (Docker Desktop or OrbStack).
#   scripts/test-databases.sh up    # start and wait
#   scripts/test-databases.sh down  # stop and remove
set -eu
cd "$(dirname "$0")/.."
case "${1:-up}" in
  up)
    docker compose -f compose.test.yaml up -d
    printf 'attente des bases'
    until docker compose -f compose.test.yaml exec -T postgres pg_isready -U postgres >/dev/null 2>&1 \
      && docker compose -f compose.test.yaml exec -T mysql mysqladmin ping -uroot -proot --silent >/dev/null 2>&1; do
      printf '.'; sleep 1
    done
    echo
    echo 'export HERDR_DB_TEST_POSTGRES="host=127.0.0.1 port=55432 user=postgres password=postgres dbname=herdr_db_test"'
    echo 'export HERDR_DB_TEST_MYSQL="host=127.0.0.1 port=53306 user=root password=root dbname=herdr_db_test"'
    ;;
  down) docker compose -f compose.test.yaml down -v ;;
  *) echo "usage: $0 [up|down]" >&2; exit 2 ;;
esac
