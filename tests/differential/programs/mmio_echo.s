# MMIO transmitter: write chars through the display port at 0xffff000c,
# ending with RARS's clear-display byte.
    .eqv TX_DATA 0xffff000c
    .text
    .globl main
main:
    li   s0, TX_DATA
    li   t1, 'm'
    sb   t1, 0(s0)
    li   t1, 'm'
    sb   t1, 0(s0)
    li   t1, 'o'
    sb   t1, 0(s0)
    li   t1, 12         # clear display
    sb   t1, 0(s0)
    li   t1, 'o'
    sb   t1, 0(s0)
    li   t1, 'k'
    sb   t1, 0(s0)
    li   a0, 0
    li   a7, 10
    ecall
