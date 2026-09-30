# Macros and eqv: parameterized add-immediate and a loop built from a macro.
    .eqv LIMIT 3
.macro push_reg(%r)
    addi sp, sp, -4
    sw   %r, 0(sp)
.end_macro
.macro pop_reg(%r)
    lw   %r, 0(sp)
    addi sp, sp, 4
.end_macro
    .text
    .globl main
main:
    push_reg(s0)
    push_reg(s1)
    li   s0, 0          # accumulator
    li   s1, LIMIT
count_up:
    beqz s1, report
    add  s0, s0, s1
    addi s1, s1, -1
    j    count_up
report:
    mv   a0, s0         # 3+2+1 = 6
    li   a7, 1
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    pop_reg(s1)
    pop_reg(s0)
    mv   a0, s0
    li   a7, 1          # s0 restored: still 6
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    li   a0, 0
    li   a7, 10
    ecall
