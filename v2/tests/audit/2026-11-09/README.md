# Audit reproduction cases

These are **failing acceptance probes**, not fixes. They are kept outside the
main Go module because the audited workflow code is an unapplied candidate.

From the repository root:

```sh
node v2/tests/audit/2026-11-09/reproduce.mjs
```

This verifies the candidate digest, copies it into a fresh temporary directory,
adds the two Go probes and runs them with one Go build job. Exit 1 and both
`AUDIT:` messages reproduce the defects. Temporary source and test repositories
are cleaned afterward. No user worktree is reset, no live agent starts, and no
Runner is restarted. An explicit candidate source path can be passed to evaluate
a fixed workflow implementation. Exit 0 then means these two probes pass, not
that the complete release gate passes. The default candidate must remain at its
recorded `.cache/implementation/coordination-2026-11-03/source` location.

`mobile-check.mjs` runs a local fake Runner/WebKit diagnostic against current
main web source, prints geometry and captures a screenshot under
`.cache/audit-2026-11-09`. It requires installed V2 npm/Playwright dependencies.
It reports measurements without asserting that a phone experience is usable;
it is not a real-iPhone test.

Observed results and the source digests are in the
[audit report](../../../audits/2026-11-09/README.md).
