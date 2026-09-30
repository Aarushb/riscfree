# Compressed instructions demo. Requires the C extension: turn on
# "Assemble compressed (c.*) instructions" in Settings (or pass --compressed
# on the CLI). Each c.* below is a 16-bit encoding that the machine expands
# to its 32-bit equivalent before executing, so both halves of the toolchain
# are exercised by this one program.
#
# Register notes: register-register c.* forms only accept the x8-x15 slice
# (a2-a5, s0, s1); c.li accepts any destination but its immediate spans
# -32 to 31. Larger constants are built by shifting, as below.
.globl main
main:
    c.addi sp, -16              # small frame
    c.li a2, 25
    c.slli a2, 2                # 25 << 2 = 100
    c.swsp a2, 12(sp)           # store through the sp-relative form
    c.lwsp a3, 12(sp)           # load it back
    c.add a3, a3, a2            # 100 + 100
    li a7, 1
    mv a0, a3
    ecall                       # prints 200
    c.li a7, 11
    c.li a0, 10
    ecall                       # newline

    # Branch on zero with the compressed compare: counts s0 down to zero.
    c.li s0, 3
count:
    c.addi s0, -1
    c.bnez s0, count
    c.li a0, 1
    li a7, 1
    ecall                       # prints 1 (loop exited with s0 == 0)
    c.li a7, 11
    c.li a0, 10
    ecall                       # newline

    c.addi16sp 16               # restore the frame
    c.li a7, 10
    ecall                       # exit 0
