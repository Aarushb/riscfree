# F/D arithmetic: constants in data, add/mul/div, print via PrintDouble's
# bits are not directly printable, so convert double to int and print.
    .data
a:  .double 2.5
b:  .double 4.0
    .text
    .globl main
main:
    fld  f0, a
    fld  f1, b
    fmul.d f2, f0, f1     # 10.0
    fadd.d f3, f2, f0     # 12.5
    fcvt.w.d a0, f3  # 12
    li   a7, 1
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    fdiv.d f4, f1, f0     # 1.6
    fcvt.w.d a0, f4  # 1
    li   a7, 1
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    feq.d t0, f2, f2      # 1
    mv   a0, t0
    li   a7, 1
    ecall
    li   a7, 11
    li   a0, 10
    ecall
    li   a0, 0
    li   a7, 10
    ecall
