# Timer interrupt demo: a handler prints a tick each time the timer fires,
# and exits on the fifth tick. Run with:
#   riscfree-cli --run --timer 200 examples/asm/interrupt_demo.s
# (In the GUI: Tools > Timer Tool, arm at 200, then run.)
    .data
tick_msg: .asciz "tick\n"
    .text
    .globl main
main:
    la   t0, handler
    li   t1, 0xfffffffc
    and  t0, t0, t1           # direct mode: pc lands on the handler base
    csrw utvec, t0
    li   t0, 5
    csrw uscratch, t0         # ticks remaining, kept in a saved register
    li   t0, 1
    csrs  ustatus, t0         # ustatus.UIE = bit 0: allow interrupts
wait_loop:
    wfi                       # park until the timer fires
    j    wait_loop

handler:
    addi sp, sp, -12
    sw   ra, 8(sp)
    sw   a0, 4(sp)
    sw   a7, 0(sp)
    la   a0, tick_msg
    li   a7, 4                # PrintString
    ecall
    csrr t6, uscratch
    addi t6, t6, -1
    csrw uscratch, t6
    lw   a7, 0(sp)
    lw   a0, 4(sp)
    lw   ra, 8(sp)
    addi sp, sp, 12
    bnez t6, handler_return
    li   a0, 0
    li   a7, 10               # fifth tick: exit from the handler
    ecall
handler_return:
    uret
