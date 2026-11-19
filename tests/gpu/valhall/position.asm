LD_ATTR_IMM.v2.f32.slot0.wait0 @r0:r1, r60^, r61^, index:0, table:1
FMA.f32 r0, r0^, u0.w0, u0.w1
FMA.f32 r1, r1^, u1.w0, u1.w1
MOV.i32 r2, u2.w0
MOV.i32 r3, 0x3f800000
LEA_BUF_IMM.slot1.wait1 @r4:r5, r59^, table:13, index:0
STORE.i128.istream.slot2.end @r0:r1:r2:r3, [r4^:r5^], offset:0
