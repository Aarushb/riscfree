# Bitmap demo: paint a red/green checkerboard at the default bitmap base.
# Open Tools > Bitmap Display, base 10010000, 16x16, then run.
    .eqv BASE 0x10010000
    .text
    .globl main
main:
    li  s0, BASE       # current pixel address
    li  s1, 16         # row
    li  s6, 0x00ff0000 # red
    li  s7, 0x0000ff00 # green
row_loop:
    li  s2, 16         # column
    xor t0, s1, s2     # row xor col decides parity
col_loop:
    andi t1, t0, 1
    beqz t1, paint_green
    mv  t2, s6
    j store_pixel
paint_green:
    mv  t2, s7
store_pixel:
    sw  t2, 0(s0)
    addi s0, s0, 4
    addi s2, s2, -1
    xori t0, t0, 1
    bnez s2, col_loop
    addi s1, s1, -1
    bnez s1, row_loop
    li  a0, 0
    li  a7, 10
    ecall
