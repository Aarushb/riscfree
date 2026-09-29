# Timer interrupt demo: prints a tick every 100000 instructions until 5 ticks.
    .data
tick_msg: .asciz "tick\n"
    .text
    .globl main
main:
    la  t0, handler
    andi t0, t0, 0xfffffffc
    csrw utvec, t0
    li  t0, 5
    csrw uscratch, t0  # remaining ticks in uscratch
    csrsi ustatus, 1   # enable user interrupts (UIE)
    li  a0, 100000
    li  a1, 0
    ecall              # placeholder: timer armed by the host/tools in phase 2
wait:
    wfi
    j   wait

handler:
    csrr t5, ucause
    la   a0, tick_msg
    li   a7, 4
    ecall
    csrr t6, uscratch
    addi t6, t6, -1
    csrw uscratch, t6
    beqz t6, stop_ticks
    uret
stop_ticks:
    csrci ustatus, 1
    li   a0, 0
    li   a7, 10
    ecall
