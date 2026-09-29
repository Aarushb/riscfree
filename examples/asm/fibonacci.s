# Print the first 12 Fibonacci numbers, one per line.
    li s0, 12          # count
    li a1, 0           # fib(0)
    li a2, 1           # fib(1)
loop:
    beqz s0, done
    mv a0, a1
    li a7, 1           # PrintInt
    ecall
    li a7, 11          # PrintChar
    li a0, 10          # newline
    ecall
    mv a3, a1
    mv a1, a2
    add a2, a3, a2
    addi s0, s0, -1
    j loop
done:
    li a7, 10          # Exit
    ecall
