# Arrays and strings: fill, sum, find max, reverse a string in place.
    .data
array:  .word 4, -9, 15, 2, -7, 15, 0
alen:   .word 7
text:   .asciz "hello"
buf:    .space 16
    .text
    .globl main
main:
    la   s0, array
    lw   t0, alen
    li   t1, 0          # sum
    li   t2, -2147483648 # max
    mv   t3, s0
sum_scan:
    beqz t0, scan_done
    lw   t4, 0(t3)
    add  t1, t1, t4
    ble  t4, t2, not_max
    mv   t2, t4
not_max:
    addi t3, t3, 4
    addi t0, t0, -1
    j    sum_scan
scan_done:
    mv   a0, t1
    li   a7, 1          # sum = 20
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    mv   a0, t2
    li   a7, 1          # max = 15
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    # reverse "hello" into buf
    la   s1, text
    la   s2, buf
    li   t0, 5
rev_loop:
    addi t1, t0, -1
    add  t2, s1, t1
    lbu  t3, 0(t2)
    sb   t3, 0(s2)
    addi s2, s2, 1
    addi t0, t0, -1
    bnez t0, rev_loop
    sb   zero, 0(s2)
    la   a0, buf
    li   a7, 4          # prints "olleh"
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    li   a0, 0
    li   a7, 10
    ecall
