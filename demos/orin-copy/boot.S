.section .text.boot, "ax"
.global _start
_start:
    b start
    .word 0
    .quad 0, 0, 0, 0, 0, 0
    .ascii "ARMd"
    .word 0

start:
    mov x0, #0x80410000
    mov sp, x0
    bl demo
1:  wfi
    b 1b
