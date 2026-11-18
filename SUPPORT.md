# Support

Lazybox is an early preview, developed in a private repository and tried by a
few invited testers.

## Questions, bugs and feature requests

In the app, **Help → Send feedback…** writes a report you read first and can
open a GitHub issue filled in with it. Or open an
[issue](https://github.com/AntoineToussaint/lazybox/issues/new) in this
repository yourself, or email
[toussaint.antoine@gmail.com](mailto:toussaint.antoine@gmail.com).

For a bug, include:

- What you did, what you expected and what happened, with a screenshot for
  anything visual.
- The output of `lazybox doctor`, and the commit you built
  (`git rev-parse --short HEAD`; Settings → About & updates shows it too). Working on
  Lazybox itself? Say whether it was Dev or Working (`make environments`).
- Your macOS version.
- For a failed `make install`, the end of its terminal output.

[docs/testers.md](docs/testers.md#how-to-report-a-problem) says where the logs
are. Read them before you attach them: they can contain paths and commands
from your work.

For a feature request, lead with the problem you are hitting, not just the
feature you have in mind.

## Documentation

Start at the [README](README.md) and [the docs](docs/README.md). Contributor
process is in [`CONTRIBUTING.md`](CONTRIBUTING.md).

Potential vulnerabilities should not be posted as issues. Follow
[`SECURITY.md`](SECURITY.md).
