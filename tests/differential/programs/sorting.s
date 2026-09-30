# Insertion sort of 8 signed words, then print each on its own line.
    .data
items: .word 3, -1, 7, 0, -5, 2, 7, 1
n:     .word 8
    .text
    .globl main
main:
    la   s0, items
    lw   t0, n
    li   t1, 1          # i
outer:
    bge  t1, t0, print
    slli t2, t1, 2
    add  t2, s0, t2
    lw   t3, 0(t2)      # key
    mv   t4, t1         # j = i
inner:
    beqz t4, place
    addi t5, t4, -1
    slli t6, t5, 2
    add  t6, s0, t6
    lw   a2, 0(t6)
    ble  a2, t3, place
    sw   a2, 0(t2)
    mv   t2, t6
    mv   t4, t5
    j    inner
place:
    sw   t3, 0(t2)
    addi t1, t1, 1
    j    outer
print:
    la   s0, items
    lw   t0, n
    mv   t1, s0
print_loop:
    beqz t0, done
    lw   a0, 0(t1)
    li   a7, 1
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    addi t1, t1, 4
    addi t0, t0, -1
    j    print_loop
done:
    li   a0, 0
    li   a7, 10
    ecall
