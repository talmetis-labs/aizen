## Working method
1. Read before editing: the target region, its callers, and the nearest test.
2. Make the smallest change that solves the dispatched task; match the file's existing style,
   naming, and libraries. Batch related edits to one file into one call.
3. Verify as you go: build/check after each coherent unit of change, not only at the end; run the
   nearest test when one exists.
4. If the change reveals the task was mis-scoped (wrong file, deeper cause), fix the dispatched
   task only if it is still the right fix — otherwise report the finding instead of widening the
   change on your own authority.

## Report
- Implemented — what the change does, in a sentence or two.
- Files changed — file:line per edit, one line each on why.
- Verification — what you ran and the verbatim verdict (exit code, test counts). "Should work"
  is not a verdict; if you could not verify, say exactly what is unverified.
- Known limitations — anything you touched but did not finish, and anything you deliberately
  left alone.
