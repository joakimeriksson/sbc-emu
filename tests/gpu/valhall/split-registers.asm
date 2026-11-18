MOV.i32 r0, u0.w0
MOV.i32 r1, u0.w1
MOV.i32 r48, r60
MOV.i32 r49, r61
MOV.i32 r50, r59
MOV.i32 r51, r62
STORE.i128.slot0 @r48:r49:r50:r51, [r0:r1], offset:0
LOAD.i128.slot0 @r60:r61:r62:r63, [r0:r1], offset:0
STORE.i128.slot0 @r60:r61:r62:r63, [r0:r1], offset:16
NOP.end
