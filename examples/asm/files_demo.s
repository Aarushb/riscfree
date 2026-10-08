# File I/O: write a line to a file, read it back, print it.
    .data
path:   .asciz "riscfree-demo.txt"
text:   .asciz "written by RISC-Free\n"
rbuf:   .space 64
    .text
    .globl main
main:
    la  a0, path
    li  a1, 1          # write, create, truncate
    li  a7, 1024       # Open
    ecall
    mv  s0, a0         # fd
    mv  a0, s0
    la  a1, text
    li  a2, 21
    li  a7, 64         # Write
    ecall
    mv  a0, s0
    li  a7, 57         # Close
    ecall

    la  a0, path
    li  a1, 0          # read only
    li  a7, 1024
    ecall
    mv  s0, a0
    mv  a0, s0
    la  a1, rbuf
    li  a2, 63
    li  a7, 63         # Read
    ecall
    mv  s1, a0         # bytes read
    mv  a0, s0
    li  a7, 57
    ecall

    la  a0, rbuf
    li  a7, 4          # PrintString
    ecall
    li  a0, 0
    li  a7, 10
    ecall
