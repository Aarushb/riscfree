# Recursion and stack discipline: factorial of 6, printed twice (recursive + loop sum check).
    .text
    .globl main
main:
    li   a0, 6
    call factorial
    mv   s1, a0         # 720
    li   a7, 1
    mv   a0, s1
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    # sum 1..6 with a loop = 21
    li   t0, 6
    li   t1, 0
sum_loop:
    add  t1, t1, t0
    addi t0, t0, -1
    bnez t0, sum_loop
    mv   a0, t1
    li   a7, 1
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    li   a0, 0
    li   a7, 10
    ecall

# n! recursive with proper prologue/epilogue
factorial:
    beqz a0, fact_base
    addi sp, sp, -8
    sw   ra, 4(sp)
    sw   a0, 0(sp)
    addi a0, a0, -1
    call factorial
    lw   t0, 0(sp)
    mul  a0, a0, t0
    lw   ra, 4(sp)
    addi sp, sp, 8
    ret
fact_base:
    li   a0, 1
    ret
