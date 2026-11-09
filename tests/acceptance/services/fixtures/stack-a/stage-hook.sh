#!/bin/sh
# The git-ignored state of stack A (run by ../stage.py, cwd = the staged project):
# data/app.sqlite (table t, one row 'seed') and public/uploads/a.bin (4 KiB,
# deterministic), and for stack-a itself public/uploads/big.bin (200 MiB of random
# bytes, for the APFS clone check). All covered by .gitignore.
set -eu
mkdir -p data public/uploads
rm -f data/app.sqlite
python3 - <<'PY'
import sqlite3
db = sqlite3.connect("data/app.sqlite")
db.execute("create table t (v text)")
db.execute("insert into t values ('seed')")
db.commit()
db.close()
PY
yes nfx-upload-a | head -c 4096 > public/uploads/a.bin
# Variants (stack-a-mixed, ...) share this hook but skip the 200 MiB file.
if [ "${NFX_FIXTURE:-stack-a}" = stack-a ]; then
  head -c 209715200 /dev/urandom > public/uploads/big.bin
fi
