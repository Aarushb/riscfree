# Differential corpus

Each `programs/<name>.s` has a sibling `<name>.expected` holding the exact
stdout of a correct run through `asaccess-cli --run`. `run_corpus.py` checks
our output against it, and — when pointed at a RARS jar with `--rars-jar` —
also against RARS itself, which is what makes a divergence visible instead of
a shared bug.

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

Expected files are generated once from a verified-correct run and then frozen;
when our behavior changes, the diff is reviewed by hand before regenerating.
