# C compressed extension: 16-bit instructions through a real program shape.
# Run with --compressed (see compressed.flags). Every c.* expands to its
# 32-bit equivalent before execution, so this exercises the assembler's
# encoders and the machine's expander together. RARS cannot run this file;
# the expected output is hand-computed from the semantics.
#
# Register notes: the register-register c.* forms (c.add, c.sub, ...) only
# accept the x8-x15 slice, so the arithmetic here uses a2 (x12), s0 (x8),
# s1 (x9), and a3 (x13). c.li and c.lwsp/c.swsp accept any register for
# their rd fields, and sp-relative offsets are unsigned multiples scaled
# by the access width.
.globl main
main:
    c.addi sp, -32              # local frame
    c.li a2, 7
    c.li s1, 5
    c.add a2, a2, s1            # 12
    c.swsp a2, 28(sp)           # store at sp+28
    c.lwsp s1, 28(sp)           # reload: 12
    c.sub s1, s1, a2            # 0
    li a7, 1
    mv a0, s1
    ecall                       # 0
    c.nop

    # s0 is in the compressed register slice; shift it, then branch on zero.
    c.li s0, 1
    c.slli s0, 4                # 16
    mv a0, s0
    ecall                       # 16
    c.beqz s1, ok               # s1 was zeroed by the sub above, so taken
    li a0, 999
    j print
ok:
    c.li a0, 1
print:
    li a7, 1
    ecall                       # 1
    c.li a7, 11
    c.li a0, 10
    ecall                       # newline

    # 100 does not fit c.li's 6-bit immediate, so build it by shifting.
    c.li a3, 25
    c.slli a3, 2                # 100
    c.swsp a3, 24(sp)
    c.lwsp a2, 24(sp)           # 100
    c.lwsp s1, 28(sp)           # 12 again
    c.add a2, a2, s1            # 112
    li a7, 1
    c.mv a0, a2
    ecall                       # 112
    c.li a7, 11
    c.li a0, 10
    ecall                       # newline

    c.addi16sp 32               # restore the frame
    c.li a7, 10
    ecall                       # exit 0
