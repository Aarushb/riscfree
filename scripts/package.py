"""Package a portable Windows zip: release binaries, prism.dll, and a
first-run note. The zip is the distribution: no installer UI, no admin
rights, updates by replacing the files.

Usage: python scripts/package.py (after `cargo build --release -p riscfree
-p riscfree-cli`); the zip lands in target/dist/.
"""

import re
import sys
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
VERSION = re.search(r'^version = "(.*)"', (REPO / "Cargo.toml").read_text(), re.M).group(1)
NAME = f"riscfree-{VERSION}-windows-x64"

FIRST_RUN = """\
RISC-Free: an accessibility-first RISC-V IDE.

Run riscfree.exe for the GUI. riscfree-cli.exe runs programs headless
(riscfree-cli --help lists the flags); both read the same assembly.

In the GUI, F1 shows every keyboard shortcut. Tab moves between panes;
F3 assembles and F5 runs. Your screen reader is spoken to directly
through Prism, so keep it running before you start the app. If speech
ever goes quiet, use File > Reconnect screen reader speech.
"""


def main() -> int:
    release = REPO / "target" / "release"
    needed = [release / "riscfree.exe", release / "riscfree-cli.exe", release / "prism.dll"]
    missing = [p for p in needed if not p.exists()]
    if missing:
        print(
            "missing " + ", ".join(p.name for p in missing)
            + "; build release first:\n"
            + "  cargo build --release -p riscfree -p riscfree-cli"
        )
        return 2
    out_dir = REPO / "target" / "dist"
    out_dir.mkdir(parents=True, exist_ok=True)
    zpath = out_dir / f"{NAME}.zip"
    with zipfile.ZipFile(zpath, "w", zipfile.ZIP_DEFLATED) as z:
        for p in needed:
            z.write(p, f"{NAME}/{p.name}")
        z.writestr(f"{NAME}/FIRST-RUN.txt", FIRST_RUN)
    print(zpath)
    return 0


if __name__ == "__main__":
    sys.exit(main())
