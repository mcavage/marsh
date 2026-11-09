#!/bin/sh
# Start the dev Postgres and seed 3 users (`<UAT_RUN>-user-N`). Idempotent.
set -eu
prefix=${UAT_RUN:-uat}
docker compose up -d --wait
docker compose exec -T db psql -U postgres -d app -v ON_ERROR_STOP=1 -q -c "create table if not exists users (id serial primary key, name text not null)"
for n in 1 2 3; do
  docker compose exec -T db psql -U postgres -d app -v ON_ERROR_STOP=1 -q -c \
    "insert into users (name) select '$prefix-user-$n' where not exists (select 1 from users where name = '$prefix-user-$n')"
done
echo "seeded: $(docker compose exec -T db psql -U postgres -d app -Atc 'select count(*) from users') users"
