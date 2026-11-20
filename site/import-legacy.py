"""Freeze a built V1 site's public archive. Usage: python3 site/import-legacy.py /path/to/web/dist"""
import gzip
import io
from pathlib import Path
import sys
import tarfile

source = Path(sys.argv[1])
if not (source / "docs/v1/index.html").is_file() or not (source / "v1/index.html").is_file():
    sys.exit("Build the archived V1 homepage and documentation entries first.")
roots = {"_astro", "demo", "docs", "v1", "pagefind", "favicon.svg", "og.png"}
target = Path(__file__).with_name("legacy.tar.gz")
with target.open("wb") as output, gzip.GzipFile(filename="", fileobj=output, mode="wb", mtime=0) as gz:
    with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for path in sorted(source.rglob("*")):
            name = path.relative_to(source)
            if not path.is_file() or name.parts[0] not in roots or str(name) == "docs/index.html":
                continue
            data = path.read_bytes()
            if str(name) == "docs/tutorials/core-workflows/index.html":
                data = data.replace(b"#create-or-improve-a-snippet-with-ask-lazybox",
                                    b"#create-or-improve-one-with-ask-lazybox")
            if str(name) == "docs/reference/configuration/index.html":
                data = data.replace(b"/docs/features/workspaces-and-worktrees/",
                                    b"/docs/explanation/mental-model/#why-a-worktree-per-workspace")
            entry = tarfile.TarInfo(name.as_posix())
            entry.size = len(data)
            archive.addfile(entry, io.BytesIO(data))
