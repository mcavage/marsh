```sh
marsh split -n \
  -b claude="claude -p 'The test fails. Fix it without editing the test.' && make test" \
  -b codex="codex exec 'The test fails. Fix it without editing the test.' && make test" |
  marsh join -- sh -c 'cat >/dev/null
    for b in claude codex; do
      if grep -q "^exited 0" "$SPLIT_DIR/$b/status"; then git apply "$SPLIT_DIR/$b/diff.patch"; exit; fi
    done
    exit 1'
```
