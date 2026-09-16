# Workflow specs

`implement.json` is the `implement` preset as a spec file: daedalus makes the change, themis
verifies it (first line `VERDICT: PASS|FAIL`; a FAIL re-dispatches daedalus once with the failure
attached), nemesis reviews the result. It targets the `bench-tasks/add-feature` crate. Run it from a
scratch copy so the fixture stays pristine:

```bash
cp -r bench-tasks/add-feature /tmp/add-feature && cd /tmp/add-feature
git init -q && git add -A && git commit -qm base
aizen workflow <repo>/bench-fixtures/workflows/implement.json --yes --trace trace.json
```

`trace.json` keeps every attempt: a `verify` FAIL shows up as `implement#2` and `verify#2` rows.
