```sh
marsh split -n \
  -b claude="claude -p 'The test fails. Fix it without editing the test.' && make test" \
  -b codex="codex exec 'The test fails. Fix it without editing the test.' && make test" | marsh join
```
