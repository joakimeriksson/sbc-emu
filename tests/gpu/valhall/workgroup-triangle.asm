MOV.i32 r0, u0.w0
MOV.i32 r1, u0.w1
MOV.i32 r15, r61
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r60
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r15, r15, r15
IADD.u32 r0, r0, r15
U32_TO_F32 r8, r60
U32_TO_F32 r9, r61
MOV.i32 r16, u1.w0
MOV.i32 r17, u1.w1
FMA.f32 r8, r8, r16, r17
MOV.i32 r16, u2.w0
MOV.i32 r17, u2.w1
FMA.f32 r9, r9, r16, r17
MOV.i32 r10, u3.w0
MOV.i32 r11, u3.w1
MOV.i32 r12, u4.w0
MOV.i32 r13, u4.w1
MOV.i32 r14, u5.w0
FMA.f32 r4, r9, r10, r11
FMA.f32 r3, r8, r12, r13
FMA.f32 r3, r4, r14, r3
FADD.f32 r2, r3.neg, 0x3f800000
FADD.f32 r2, r4.neg, r2
FCMP_OR.f32.lt.i1 r6, r2, 0x00000000, 0x00000000
FCMP_OR.f32.lt.i1 r6, r3, 0x00000000, r6
FCMP_OR.f32.lt.i1 r6, r4, 0x00000000, r6
BRANCHZ r6, offset:3
MOV.i32 r5, 0x3f800000
STORE.i128.slot0 @r2:r3:r4:r5, [r0:r1], offset:0
NOP.end
MOV.i32 r2, u6.w0
MOV.i32 r3, u6.w1
MOV.i32 r4, u7.w0
MOV.i32 r5, 0x3f800000
STORE.i128.slot0 @r2:r3:r4:r5, [r0:r1], offset:0
NOP.end
