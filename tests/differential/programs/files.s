# File round trip through the RARS-numbered file syscalls.
    .data
path:  .asciz "corpus_file.txt"
msg:   .asciz "corpus says hi"
back:  .space 32
    .text
    .globl main
main:
    la   a0, path
    li   a1, 1          # write, create, truncate
    li   a7, 1024
    ecall
    mv   s0, a0
    mv   a0, s0
    la   a1, msg
    li   a2, 14
    li   a7, 64         # Write
    ecall
    mv   a0, s0
    li   a7, 57         # Close
    ecall
    la   a0, path
    li   a1, 0
    li   a7, 1024
    ecall
    mv   s0, a0
    mv   a0, s0
    la   a1, back
    li   a2, 31
    li   a7, 63         # Read
    ecall
    mv   a0, s0
    li   a7, 57
    ecall
    la   a0, back
    li   a7, 4
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    li   a0, 0
    li   a7, 10
    ecall
