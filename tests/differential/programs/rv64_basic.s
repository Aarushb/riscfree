# RV64 basics: wide constants, doubleword stack traffic, w-suffix semantics.
# Prints only values whose low 32 bits are meaningful so the run stays
# comparable against RARS's print-int syscall.
.globl main
main:
    # A constant above 32 bits forces the wide multi-instruction li chain.
    li t0, 0x123456789
    srli t1, t0, 32             # high half
    li a7, 1
    mv a0, t1
    ecall                       # 1
    jal ra, newline

    li t2, 0xffffffff
    and t3, t0, t2              # low half: 0x23456789
    li a7, 1
    mv a0, t3
    ecall                       # 591751049
    jal ra, newline

    # Doubleword store/load roundtrip through the stack.
    addi sp, sp, -16
    sd t0, 0(sp)
    ld t4, 0(sp)
    addi sp, sp, 16
    sub t5, t4, t0
    li a7, 1
    mv a0, t5
    ecall                       # 0
    jal ra, newline

    # addiw wraps at 32 bits and sign-extends the result into the register.
    li t0, 0x7fffffff
    addiw t1, t0, 1             # 0xffffffff80000000
    srai t2, t1, 32             # arithmetic shift keeps the sign: -1
    li a7, 1
    mv a0, t2
    ecall                       # -1
    jal ra, newline

    # w-shifts operate on 32 bits: slliw wraps, srliw shifts logically.
    li t0, 0x80000000
    slliw t1, t0, 1             # (0x80000000 << 1) mod 2^32 = 0
    li a7, 1
    mv a0, t1
    ecall                       # 0
    jal ra, newline

    srliw t2, t0, 28            # 0x80000000 >> 28 = 8
    li a7, 1
    mv a0, t2
    ecall                       # 8
    jal ra, newline

    li a7, 10
    ecall

newline:
    li a7, 11
    li a0, 10
    ecall
    ret
