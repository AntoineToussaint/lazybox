<!--
Title: Conventional Commits (`feat: …`, `fix: …`, `docs: …`), saying what a
person can now do. AGENTS.md and CONTRIBUTING.md have the rules this follows.
-->

## What changes

<!-- What a person can now do, or what was wrong and is now right. One line per change. -->

## Fix or hack

<!--
Classify every change that makes something work:
- fix: at the place that owns the behaviour;
- hack: a stopgap somewhere else, with the issue or pull request (here or in
  the owning repository) for the real fix.
-->

## How it was verified

<!-- The tests added or changed (a bug fix has one that fails without it), and what you ran: make check, make check-desktop, UI gates by id. -->

## Not verified

<!-- What you could not exercise (no provider login, no phone, no signing keys, a gate you skipped), or "Nothing". Unverified is not the same as working. -->

## Checklist

- [ ] Docs that describe the changed behaviour are updated.
- [ ] A user-visible change has its line in CHANGELOG.md and `make release-notes` was run.
- [ ] No generated code edited by hand (`make check-generated` passes).
