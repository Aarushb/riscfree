# Differential corpus

Each `programs/<name>.s` has a sibling `<name>.expected` holding the exact
stdout of a correct run through `riscfree-cli --run`. `run_corpus.py` checks
our output against it, and — when pointed at a RARS jar with `--rars-jar` —
also against RARS itself, which is what makes a divergence visible instead of
a shared bug.

Programs needing CLI flags beyond the defaults (RV64 mode, the C extension)
carry a `<name>.flags` sidecar whose whitespace-separated flags are passed
through to the CLI. RARS comparison is skipped in spirit for such programs:
RARS has no RV64 C-extension path, so their expected files are hand-computed
rather than generated from a RARS run.

Corpus programs must be deterministic: no wall-clock syscalls, no unscripted
input, seeded randomness only. Every program exits with code 0 through the
exit syscall.

| Program | Exercises |
|---|---|
| `fibonacci` | loops, arithmetic, print int/char syscalls (also in `examples/asm`) |
| `functions` | call/ret, stack prologue/epilogue, recursion |
| `arrays` | data directives, indexed loads/stores, string traversal |
| `sorting` | nested loops, signed comparisons, in-place swaps |
| `floats` | F/D arithmetic, conversions, print float/double |
| `files` | open/write/close/read round-trip |
| `mmio_echo` | transmitter data port writes (clear-display included) |
| `macros` | `.macro` expansion, `.eqv` interaction |
| `rv64_basic` | RV64 mode: ld/sd, addiw, w-suffix ops, wide li |
| `compressed` | C extension: slice-register arithmetic, sp-relative store/load, branches, shifts (runs via `--compressed`) |

Expected files are generated once from a verified-correct run and then frozen;
when our behavior changes, the diff is reviewed by hand before regenerating.
