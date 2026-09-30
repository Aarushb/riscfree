"""RARS differential harness: run each corpus program through both RARS and
asaccess-cli and compare observable behavior (stdout, exit path, final
registers). This is the parity arbiter for ambiguous semantics (see
docs/COMPAT.md).

Usage: python run_corpus.py [--rars-jar path] [--only name]

Requirements: a Java runtime and rars.jar for the RARS side. Every corpus
program must be deterministic: no wall-time syscalls, no MMIO keyboard input
unless scripted, seeded randomness only. See corpus.md for the program list
and what each one exercises.
"""

import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CLI = REPO / "target" / "debug" / "asaccess-cli.exe"
CORPUS = Path(__file__).parent / "programs"


def stdin_for(program: Path) -> str:
    # Optional <name>.input feeds programs that read console input.
    input_file = program.with_suffix(".input")
    return input_file.read_text() if input_file.exists() else ""


def flags_for(program: Path) -> list[str]:
    # Optional <name>.flags holds extra CLI flags (e.g. --rv64, --compressed),
    # whitespace-separated, inserted before the program path.
    flags_file = program.with_suffix(".flags")
    return flags_file.read_text().split() if flags_file.exists() else []


def run_cli(program: Path) -> tuple[str, int]:
    result = subprocess.run(
        [str(CLI), "--run", *flags_for(program), str(program)],
        capture_output=True,
        text=True,
        timeout=60,
        input=stdin_for(program),
    )
    return result.stdout, result.returncode


def run_rars(jar: Path, program: Path) -> tuple[str, int]:
    # `ic` prints the instruction count; register dumps come from `reg` args
    # in real use. Start with stdout comparison, which is what coursework
    # actually grades.
    result = subprocess.run(
        ["java", "-jar", str(jar), str(program)],
        capture_output=True,
        text=True,
        timeout=60,
        input=stdin_for(program),
    )
    return result.stdout, result.returncode


def main() -> int:
    args = sys.argv[1:]
    jar = None
    if "--rars-jar" in args:
        jar = Path(args[args.index("--rars-jar") + 1])
    only = args[args.index("--only") + 1] if "--only" in args else None

    if not CLI.exists():
        print(f"ERROR: build the CLI first: {CLI} missing", file=sys.stderr)
        return 2

    programs = sorted(CORPUS.glob("*.s")) if only is None else [CORPUS / f"{only}.s"]
    failures = 0
    for program in programs:
        expected = program.with_suffix(".expected")
        if not expected.exists():
            print(f"SKIP {program.name} (no .expected file)")
            continue
        expected_text = expected.read_text()
        stdout, code = run_cli(program)
        ok = stdout == expected_text and code == 0
        state = "PASS" if ok else "FAIL"
        print(f"{state} {program.name}")
        if not ok:
            failures += 1
            print(f"  expected: {expected_text!r}")
            print(f"  actual:   {stdout!r} (exit {code})")
        if jar is not None:
            rars_out, _ = run_rars(jar, program)
            if rars_out != stdout:
                failures += 1
                print(f"  DIVERGENCE vs RARS: {rars_out!r}")

    print(f"{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
