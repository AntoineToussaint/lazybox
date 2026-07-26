# Support

lazybox is pre-1.0 and maintained in the open. Here's where to go depending on
what you need.

## Questions & help

For "how do I…", setup help, or sharing your configuration, start a
[question / setup help report](https://github.com/AntoineToussaint/lazybox/issues/new?template=question.yml).

## Bugs & feature requests

Open an [issue](https://github.com/AntoineToussaint/lazybox/issues/new/choose):

- **Bug report** — something is broken. Include a `/tmp/lazybox.log` excerpt
  (re-run with `RUST_LOG=lazybox=debug` for more), your OS, and the version
  you're running (`lazybox --version`; for source builds, the commit you built
  from).
- **Feature request** — describe the problem you're hitting, not just the
  feature you have in mind.

## Documentation

The docs site lives at <https://lazybox.ai/docs/>, and the
architecture notes are in [`CLAUDE.md`](./CLAUDE.md) and `DESIGN.md`.

Potential vulnerabilities should not be posted as public issues. Follow
[`SECURITY.md`](SECURITY.md) to open a private GitHub security advisory.

## A note on response times

This is an early-adopter, pre-1.0 project — releases ship via the Homebrew tap,
the `curl | sh` installer, and GitHub Releases, but support is best-effort and
replies may take a while. The more detail you include up front,
the faster we can help.
