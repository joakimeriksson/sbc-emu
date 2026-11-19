MOV.i32 r0, u0.w0
MOV.i32 r1, u0.w1
IADD.u32 r6, r60, r61
IADD.u32 r6, r6, r6
IADD.u32 r6, r6, r6
IADD.u32 r6, r6, r6
IADD.u32 r6, r6, r6
IADD.u32 r0, r0, r6
LD_ATTR_IMM.v2.f32.slot0 @r2:r3, r60^, r61^, index:0, table:1
MOV.i32 r4, 0x00000000
MOV.i32 r5, 0x3f800000
STORE.i128.slot0 @r2:r3:r4:r5, [r0:r1], offset:0
NOP.end
