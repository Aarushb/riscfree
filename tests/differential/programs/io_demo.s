# Console I/O: echo a name and a number through the read/print syscalls.
    .data
prompt: .asciz "What is your name? "
greet:  .asciz "Hello, "
newline: .asciz "\n"
name:   .space 64
    .text
    .globl main
main:
    la  a0, prompt
    li  a7, 4          # PrintString
    ecall
    la  a0, name
    li  a1, 64
    li  a7, 8          # ReadString
    ecall
    la  a0, greet
    li  a7, 4
    ecall
    la  a0, name
    li  a7, 4
    ecall
    la  a0, newline
    li  a7, 4
    ecall
    li  a0, 0
    li  a7, 10         # Exit
    ecall
