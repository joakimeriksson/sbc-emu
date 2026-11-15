#!/usr/bin/env python3
"""Generate arm-a32/tests/neon/cases.rs: Advanced SIMD instructions assembled as A32 and as
T32 by arm-none-eabi-as, each paired with an AArch64 snippet that computes the same result
on the host (arm-a32/tests/neon_diff.rs runs both and compares).

Usage: python3 tools/gen_neon_cases.py > arm-a32/tests/neon/cases.rs

Register mapping: A32 q<n> (d<2n>, d<2n+1>) is host v<n>, so an A32 D-form on d<2k> is the
host's 64-bit form on v<k>; odd D registers need the host's "2" (upper half) forms or
explicit moves. v16-v31 are host scratch. A32 r0-r7 are host w0-w7; memory forms use
[r0] (a 32-byte aligned buffer) and r1 as the post-index register.

Each line: A32 instruction | host instructions separated by ';' | options
Options: dest=d0,d2,r0 (registers the instruction writes; default: the first operand,
both for VSWP/VTRN/VUZP/VZIP, the list for VLDn), noinf (inputs without infinities).
"""
import os
import re
import subprocess
import sys
import tempfile

CASES = r"""
# ---- three registers, same length: integer
vhadd.s8 q0, q1, q2 | shadd v0.16b, v1.16b, v2.16b
vhadd.u16 d0, d2, d4 | uhadd v0.4h, v1.4h, v2.4h
vhadd.u32 q3, q4, q5 | uhadd v3.4s, v4.4s, v5.4s
vrhadd.u8 q0, q1, q2 | urhadd v0.16b, v1.16b, v2.16b
vrhadd.s32 d0, d2, d4 | srhadd v0.2s, v1.2s, v2.2s
vrhadd.s16 q15, q14, q13 | srhadd v15.8h, v14.8h, v13.8h
vqadd.s8 q0, q1, q2 | sqadd v0.16b, v1.16b, v2.16b
vqadd.u16 q0, q1, q2 | uqadd v0.8h, v1.8h, v2.8h
vqadd.s32 d0, d2, d4 | sqadd v0.2s, v1.2s, v2.2s
vqadd.u64 q0, q1, q2 | uqadd v0.2d, v1.2d, v2.2d
vqadd.s64 d0, d2, d4 | sqadd d0, d1, d2
vhsub.s16 q0, q1, q2 | shsub v0.8h, v1.8h, v2.8h
vhsub.u8 d0, d2, d4 | uhsub v0.8b, v1.8b, v2.8b
vqsub.u8 q0, q1, q2 | uqsub v0.16b, v1.16b, v2.16b
vqsub.s16 q0, q1, q2 | sqsub v0.8h, v1.8h, v2.8h
vqsub.u32 d0, d2, d4 | uqsub v0.2s, v1.2s, v2.2s
vqsub.s64 q0, q1, q2 | sqsub v0.2d, v1.2d, v2.2d
vcgt.s8 q0, q1, q2 | cmgt v0.16b, v1.16b, v2.16b
vcgt.u16 q0, q1, q2 | cmhi v0.8h, v1.8h, v2.8h
vcge.s32 q0, q1, q2 | cmge v0.4s, v1.4s, v2.4s
vcge.u8 d0, d2, d4 | cmhs v0.8b, v1.8b, v2.8b
vshl.s8 q0, q1, q2 | sshl v0.16b, v1.16b, v2.16b
vshl.u16 q0, q1, q2 | ushl v0.8h, v1.8h, v2.8h
vshl.s32 d0, d2, d4 | sshl v0.2s, v1.2s, v2.2s
vshl.u64 q0, q1, q2 | ushl v0.2d, v1.2d, v2.2d
vshl.s64 d0, d2, d4 | sshl d0, d1, d2
vqshl.s8 q0, q1, q2 | sqshl v0.16b, v1.16b, v2.16b
vqshl.u16 q0, q1, q2 | uqshl v0.8h, v1.8h, v2.8h
vqshl.s32 q0, q1, q2 | sqshl v0.4s, v1.4s, v2.4s
vqshl.u64 q0, q1, q2 | uqshl v0.2d, v1.2d, v2.2d
vrshl.s8 q0, q1, q2 | srshl v0.16b, v1.16b, v2.16b
vrshl.u32 q0, q1, q2 | urshl v0.4s, v1.4s, v2.4s
vrshl.s64 q0, q1, q2 | srshl v0.2d, v1.2d, v2.2d
vqrshl.s16 q0, q1, q2 | sqrshl v0.8h, v1.8h, v2.8h
vqrshl.u8 d0, d2, d4 | uqrshl v0.8b, v1.8b, v2.8b
vqrshl.s64 q0, q1, q2 | sqrshl v0.2d, v1.2d, v2.2d
vqrshl.u32 q0, q1, q2 | uqrshl v0.4s, v1.4s, v2.4s
vmax.s8 q0, q1, q2 | smax v0.16b, v1.16b, v2.16b
vmax.u16 d0, d2, d4 | umax v0.4h, v1.4h, v2.4h
vmin.s32 q0, q1, q2 | smin v0.4s, v1.4s, v2.4s
vmin.u8 q0, q1, q2 | umin v0.16b, v1.16b, v2.16b
vabd.s8 q0, q1, q2 | sabd v0.16b, v1.16b, v2.16b
vabd.u16 q0, q1, q2 | uabd v0.8h, v1.8h, v2.8h
vabd.s32 d0, d2, d4 | sabd v0.2s, v1.2s, v2.2s
vaba.u8 q0, q1, q2 | uaba v0.16b, v1.16b, v2.16b
vaba.s16 d0, d2, d4 | saba v0.4h, v1.4h, v2.4h
vaba.u32 q0, q1, q2 | uaba v0.4s, v1.4s, v2.4s
vadd.i8 q0, q1, q2 | add v0.16b, v1.16b, v2.16b
vadd.i16 d0, d2, d4 | add v0.4h, v1.4h, v2.4h
vadd.i32 q15, q14, q13 | add v15.4s, v14.4s, v13.4s
vadd.i64 q0, q1, q2 | add v0.2d, v1.2d, v2.2d
vsub.i8 d0, d2, d4 | sub v0.8b, v1.8b, v2.8b
vsub.i64 d0, d2, d4 | sub d0, d1, d2
vsub.i32 q0, q1, q2 | sub v0.4s, v1.4s, v2.4s
vtst.8 q0, q1, q2 | cmtst v0.16b, v1.16b, v2.16b
vtst.16 d0, d2, d4 | cmtst v0.4h, v1.4h, v2.4h
vtst.32 q0, q1, q2 | cmtst v0.4s, v1.4s, v2.4s
vceq.i8 q0, q1, q2 | cmeq v0.16b, v1.16b, v2.16b
vceq.i16 q0, q1, q2 | cmeq v0.8h, v1.8h, v2.8h
vceq.i32 d0, d2, d4 | cmeq v0.2s, v1.2s, v2.2s
vmla.i8 q0, q1, q2 | mla v0.16b, v1.16b, v2.16b
vmla.i16 d0, d2, d4 | mla v0.4h, v1.4h, v2.4h
vmla.i32 q0, q1, q2 | mla v0.4s, v1.4s, v2.4s
vmls.i16 q0, q1, q2 | mls v0.8h, v1.8h, v2.8h
vmls.i8 d0, d2, d4 | mls v0.8b, v1.8b, v2.8b
vmul.i8 q0, q1, q2 | mul v0.16b, v1.16b, v2.16b
vmul.i16 q0, q1, q2 | mul v0.8h, v1.8h, v2.8h
vmul.i32 d0, d2, d4 | mul v0.2s, v1.2s, v2.2s
vmul.p8 q0, q1, q2 | pmul v0.16b, v1.16b, v2.16b
vmul.p8 d0, d2, d4 | pmul v0.8b, v1.8b, v2.8b
vpmax.s8 d0, d2, d4 | smaxp v0.8b, v1.8b, v2.8b
vpmax.u16 d0, d2, d4 | umaxp v0.4h, v1.4h, v2.4h
vpmax.s32 d0, d2, d4 | smaxp v0.2s, v1.2s, v2.2s
vpmin.u8 d0, d2, d4 | uminp v0.8b, v1.8b, v2.8b
vpmin.s16 d0, d2, d4 | sminp v0.4h, v1.4h, v2.4h
vpmin.u32 d0, d2, d4 | uminp v0.2s, v1.2s, v2.2s
vqdmulh.s16 q0, q1, q2 | sqdmulh v0.8h, v1.8h, v2.8h
vqdmulh.s32 d0, d2, d4 | sqdmulh v0.2s, v1.2s, v2.2s
vqrdmulh.s16 d0, d2, d4 | sqrdmulh v0.4h, v1.4h, v2.4h
vqrdmulh.s32 q0, q1, q2 | sqrdmulh v0.4s, v1.4s, v2.4s
vpadd.i8 d0, d2, d4 | addp v0.8b, v1.8b, v2.8b
vpadd.i16 d0, d2, d4 | addp v0.4h, v1.4h, v2.4h
vpadd.i32 d0, d2, d4 | addp v0.2s, v1.2s, v2.2s
vand q0, q1, q2 | and v0.16b, v1.16b, v2.16b
vbic d0, d2, d4 | bic v0.8b, v1.8b, v2.8b
vorr q0, q1, q2 | orr v0.16b, v1.16b, v2.16b
vmov q0, q1 | mov v0.16b, v1.16b
vorn q0, q1, q2 | orn v0.16b, v1.16b, v2.16b
veor d0, d2, d4 | eor v0.8b, v1.8b, v2.8b
vbsl q0, q1, q2 | bsl v0.16b, v1.16b, v2.16b
vbit q0, q1, q2 | bit v0.16b, v1.16b, v2.16b
vbif d0, d2, d4 | bif v0.8b, v1.8b, v2.8b
# ---- three registers, same length: floating point
vadd.f32 q0, q1, q2 | fadd v0.4s, v1.4s, v2.4s
vadd.f32 d0, d2, d4 | fadd v0.2s, v1.2s, v2.2s
vsub.f32 q0, q1, q2 | fsub v0.4s, v1.4s, v2.4s
vpadd.f32 d0, d2, d4 | faddp v0.2s, v1.2s, v2.2s
vabd.f32 q0, q1, q2 | fabd v0.4s, v1.4s, v2.4s
vmul.f32 q0, q1, q2 | fmul v0.4s, v1.4s, v2.4s
vmul.f32 d0, d2, d4 | fmul v0.2s, v1.2s, v2.2s
vmla.f32 q0, q1, q2 | fmul v16.4s, v1.4s, v2.4s ; fadd v0.4s, v0.4s, v16.4s
vmls.f32 q0, q1, q2 | fmul v16.4s, v1.4s, v2.4s ; fsub v0.4s, v0.4s, v16.4s
vmla.f32 d0, d2, d4 | fmul v16.2s, v1.2s, v2.2s ; fadd v0.2s, v0.2s, v16.2s
vfma.f32 q0, q1, q2 | fmla v0.4s, v1.4s, v2.4s
vfma.f32 d0, d2, d4 | fmla v0.2s, v1.2s, v2.2s
vfms.f32 q0, q1, q2 | fmls v0.4s, v1.4s, v2.4s
vceq.f32 q0, q1, q2 | fcmeq v0.4s, v1.4s, v2.4s
vcge.f32 q0, q1, q2 | fcmge v0.4s, v1.4s, v2.4s
vcgt.f32 d0, d2, d4 | fcmgt v0.2s, v1.2s, v2.2s
vacge.f32 q0, q1, q2 | facge v0.4s, v1.4s, v2.4s
vacgt.f32 q0, q1, q2 | facgt v0.4s, v1.4s, v2.4s
vmax.f32 q0, q1, q2 | fmax v0.4s, v1.4s, v2.4s
vmin.f32 q0, q1, q2 | fmin v0.4s, v1.4s, v2.4s
vpmax.f32 d0, d2, d4 | fmaxp v0.2s, v1.2s, v2.2s
vpmin.f32 d0, d2, d4 | fminp v0.2s, v1.2s, v2.2s
vrecps.f32 q0, q1, q2 | fmul v16.4s, v1.4s, v2.4s ; fmov v17.4s, #2.0 ; fsub v0.4s, v17.4s, v16.4s | noinf
vrsqrts.f32 q0, q1, q2 | fmul v16.4s, v1.4s, v2.4s ; fmov v17.4s, #3.0 ; fsub v16.4s, v17.4s, v16.4s ; fmov v17.4s, #0.5 ; fmul v0.4s, v16.4s, v17.4s | noinf
# ---- three registers, different lengths
vaddl.s8 q0, d2, d4 | saddl v0.8h, v1.8b, v2.8b
vaddl.u16 q0, d2, d4 | uaddl v0.4s, v1.4h, v2.4h
vaddl.s32 q0, d3, d5 | saddl2 v0.2d, v1.4s, v2.4s
vaddw.s32 q0, q1, d4 | saddw v0.2d, v1.2d, v2.2s
vaddw.u8 q0, q1, d5 | uaddw2 v0.8h, v1.8h, v2.16b
vsubl.u8 q0, d2, d4 | usubl v0.8h, v1.8b, v2.8b
vsubl.s16 q0, d2, d4 | ssubl v0.4s, v1.4h, v2.4h
vsubw.u16 q0, q1, d4 | usubw v0.4s, v1.4s, v2.4h
vsubw.s32 q0, q1, d4 | ssubw v0.2d, v1.2d, v2.2s
vaddhn.i16 d0, q1, q2 | addhn v0.8b, v1.8h, v2.8h
vaddhn.i64 d1, q1, q2 | addhn2 v0.4s, v1.2d, v2.2d
vraddhn.i32 d0, q1, q2 | raddhn v0.4h, v1.4s, v2.4s
vsubhn.i64 d0, q1, q2 | subhn v0.2s, v1.2d, v2.2d
vrsubhn.i16 d0, q1, q2 | rsubhn v0.8b, v1.8h, v2.8h
vabal.s8 q0, d2, d4 | sabal v0.8h, v1.8b, v2.8b
vabal.u32 q0, d2, d4 | uabal v0.2d, v1.2s, v2.2s
vabdl.u16 q0, d2, d4 | uabdl v0.4s, v1.4h, v2.4h
vabdl.s8 q0, d2, d4 | sabdl v0.8h, v1.8b, v2.8b
vmlal.s16 q0, d2, d4 | smlal v0.4s, v1.4h, v2.4h
vmlal.u8 q0, d2, d4 | umlal v0.8h, v1.8b, v2.8b
vmlsl.s32 q0, d2, d4 | smlsl v0.2d, v1.2s, v2.2s
vmlsl.u16 q0, d3, d5 | umlsl2 v0.4s, v1.8h, v2.8h
vqdmlal.s16 q0, d2, d4 | sqdmlal v0.4s, v1.4h, v2.4h
vqdmlal.s32 q0, d2, d4 | sqdmlal v0.2d, v1.2s, v2.2s
vqdmlsl.s32 q0, d2, d4 | sqdmlsl v0.2d, v1.2s, v2.2s
vqdmlsl.s16 q0, d2, d4 | sqdmlsl v0.4s, v1.4h, v2.4h
vmull.s8 q0, d2, d4 | smull v0.8h, v1.8b, v2.8b
vmull.u16 q0, d2, d4 | umull v0.4s, v1.4h, v2.4h
vmull.u32 q0, d2, d4 | umull v0.2d, v1.2s, v2.2s
vmull.s32 q8, d30, d31 | ext v16.16b, v15.16b, v15.16b, #8 ; smull v8.2d, v15.2s, v16.2s
vqdmull.s16 q0, d2, d4 | sqdmull v0.4s, v1.4h, v2.4h
vqdmull.s32 q0, d2, d4 | sqdmull v0.2d, v1.2s, v2.2s
vmull.p8 q0, d2, d4 | pmull v0.8h, v1.8b, v2.8b
# ---- two registers and a scalar
vmul.i16 q0, q1, d4[2] | mul v0.8h, v1.8h, v2.h[2]
vmul.i16 d0, d2, d5[3] | mul v0.4h, v1.4h, v2.h[7]
vmul.i32 d0, d2, d9[1] | mul v0.2s, v1.2s, v4.s[3]
vmul.i32 q0, q1, d15[0] | mul v0.4s, v1.4s, v7.s[2]
vmla.i16 q0, q1, d7[1] | mla v0.8h, v1.8h, v3.h[5]
vmla.i32 d0, d2, d4[0] | mla v0.2s, v1.2s, v2.s[0]
vmls.i32 q0, q1, d4[1] | mls v0.4s, v1.4s, v2.s[1]
vmls.i16 d0, d2, d4[3] | mls v0.4h, v1.4h, v2.h[3]
vmul.f32 q0, q1, d5[0] | fmul v0.4s, v1.4s, v2.s[2]
vmul.f32 d0, d2, d4[1] | fmul v0.2s, v1.2s, v2.s[1]
vmla.f32 q0, q1, d4[1] | fmul v16.4s, v1.4s, v2.s[1] ; fadd v0.4s, v0.4s, v16.4s
vmls.f32 d0, d2, d5[1] | fmul v16.2s, v1.2s, v2.s[3] ; fsub v0.2s, v0.2s, v16.2s
vmlal.s16 q0, d2, d4[3] | smlal v0.4s, v1.4h, v2.h[3]
vmlal.u32 q0, d2, d4[1] | umlal v0.2d, v1.2s, v2.s[1]
vmlsl.s32 q0, d2, d5[0] | smlsl v0.2d, v1.2s, v2.s[2]
vmlsl.u16 q0, d2, d4[0] | umlsl v0.4s, v1.4h, v2.h[0]
vqdmlal.s16 q0, d2, d4[2] | sqdmlal v0.4s, v1.4h, v2.h[2]
vqdmlsl.s32 q0, d2, d4[1] | sqdmlsl v0.2d, v1.2s, v2.s[1]
vmull.u16 q0, d2, d4[1] | umull v0.4s, v1.4h, v2.h[1]
vmull.s32 q0, d2, d4[0] | smull v0.2d, v1.2s, v2.s[0]
vqdmull.s32 q0, d2, d5[1] | sqdmull v0.2d, v1.2s, v2.s[3]
vqdmull.s16 q0, d2, d4[3] | sqdmull v0.4s, v1.4h, v2.h[3]
vqdmulh.s16 q0, q1, d4[1] | sqdmulh v0.8h, v1.8h, v2.h[1]
vqdmulh.s32 d0, d2, d4[1] | sqdmulh v0.2s, v1.2s, v2.s[1]
vqrdmulh.s32 q0, q1, d4[0] | sqrdmulh v0.4s, v1.4s, v2.s[0]
vqrdmulh.s16 d0, d2, d4[2] | sqrdmulh v0.4h, v1.4h, v2.h[2]
# ---- two registers and a shift amount
vshr.s8 q0, q1, #3 | sshr v0.16b, v1.16b, #3
vshr.u16 q0, q1, #16 | ushr v0.8h, v1.8h, #16
vshr.s32 d0, d2, #32 | sshr v0.2s, v1.2s, #32
vshr.u64 q0, q1, #64 | ushr v0.2d, v1.2d, #64
vshr.s64 d0, d2, #1 | sshr d0, d1, #1
vsra.s8 q0, q1, #8 | ssra v0.16b, v1.16b, #8
vsra.u32 q0, q1, #5 | usra v0.4s, v1.4s, #5
vrshr.s16 q0, q1, #7 | srshr v0.8h, v1.8h, #7
vrshr.u64 q0, q1, #64 | urshr v0.2d, v1.2d, #64
vrshr.u8 d0, d2, #1 | urshr v0.8b, v1.8b, #1
vrsra.u8 q0, q1, #1 | ursra v0.16b, v1.16b, #1
vrsra.s64 q0, q1, #33 | srsra v0.2d, v1.2d, #33
vsri.8 q0, q1, #3 | sri v0.16b, v1.16b, #3
vsri.16 d0, d2, #16 | sri v0.4h, v1.4h, #16
vsri.64 q0, q1, #64 | sri v0.2d, v1.2d, #64
vsri.32 q0, q1, #31 | sri v0.4s, v1.4s, #31
vshl.i8 q0, q1, #7 | shl v0.16b, v1.16b, #7
vshl.i64 q0, q1, #63 | shl v0.2d, v1.2d, #63
vshl.i16 d0, d2, #0 | shl v0.4h, v1.4h, #0
vsli.32 q0, q1, #0 | sli v0.4s, v1.4s, #0
vsli.16 q0, q1, #9 | sli v0.8h, v1.8h, #9
vsli.64 d0, d2, #40 | sli d0, d1, #40
vqshl.s8 q0, q1, #3 | sqshl v0.16b, v1.16b, #3
vqshl.u64 q0, q1, #63 | uqshl v0.2d, v1.2d, #63
vqshl.s32 d0, d2, #17 | sqshl v0.2s, v1.2s, #17
vqshl.u16 q0, q1, #0 | uqshl v0.8h, v1.8h, #0
vqshlu.s16 q0, q1, #4 | sqshlu v0.8h, v1.8h, #4
vqshlu.s64 q0, q1, #1 | sqshlu v0.2d, v1.2d, #1
vqshlu.s8 d0, d2, #7 | sqshlu v0.8b, v1.8b, #7
vshrn.i16 d0, q1, #3 | shrn v0.8b, v1.8h, #3
vshrn.i64 d1, q1, #32 | shrn2 v0.4s, v1.2d, #32
vrshrn.i32 d0, q1, #16 | rshrn v0.4h, v1.4s, #16
vrshrn.i64 d0, q1, #1 | rshrn v0.2s, v1.2d, #1
vqshrn.s16 d0, q1, #8 | sqshrn v0.8b, v1.8h, #8
vqshrn.u32 d0, q1, #3 | uqshrn v0.4h, v1.4s, #3
vqrshrn.s64 d0, q1, #17 | sqrshrn v0.2s, v1.2d, #17
vqrshrn.u16 d0, q1, #1 | uqrshrn v0.8b, v1.8h, #1
vqshrun.s16 d0, q1, #3 | sqshrun v0.8b, v1.8h, #3
vqrshrun.s32 d0, q1, #9 | sqrshrun v0.4h, v1.4s, #9
vqshrun.s64 d0, q1, #32 | sqshrun v0.2s, v1.2d, #32
vshll.s8 q0, d2, #3 | sshll v0.8h, v1.8b, #3
vshll.u16 q0, d2, #15 | ushll v0.4s, v1.4h, #15
vshll.s32 q0, d3, #1 | sshll2 v0.2d, v1.4s, #1
vmovl.s8 q0, d2 | sxtl v0.8h, v1.8b
vmovl.u32 q0, d2 | uxtl v0.2d, v1.2s
vmovl.u16 q0, d3 | uxtl2 v0.4s, v1.8h
vshll.i8 q0, d2, #8 | shll v0.8h, v1.8b, #8
vshll.i32 q0, d2, #32 | shll v0.2d, v1.2s, #32
vshll.i16 q0, d3, #16 | shll2 v0.4s, v1.8h, #16
vcvt.s32.f32 q0, q1, #8 | fcvtzs v0.4s, v1.4s, #8
vcvt.u32.f32 q0, q1, #32 | fcvtzu v0.4s, v1.4s, #32
vcvt.f32.s32 q0, q1, #16 | scvtf v0.4s, v1.4s, #16
vcvt.f32.u32 d0, d2, #1 | ucvtf v0.2s, v1.2s, #1
vcvt.s32.f32 d0, d2, #1 | fcvtzs v0.2s, v1.2s, #1
vcvt.f32.s32 q0, q1, #32 | scvtf v0.4s, v1.4s, #32
# ---- one register and a modified immediate
vmov.i8 q0, #0x5a | movi v0.16b, #0x5a
vmov.i16 d0, #0x3400 | movi v0.4h, #0x34, lsl #8
vmov.i16 q0, #0x81 | movi v0.8h, #0x81
vmov.i32 q0, #0x00120000 | movi v0.4s, #0x12, lsl #16
vmov.i32 d0, #0xab000000 | movi v0.2s, #0xab, lsl #24
vmov.i32 q0, #0x12ff | movi v0.4s, #0x12, msl #8
vmov.i32 q0, #0x12ffff | movi v0.4s, #0x12, msl #16
vmov.i64 q0, #0xff00ff0000ffff00 | movi v0.2d, #0xff00ff0000ffff00
vmov.f32 q0, #1.5 | fmov v0.4s, #1.5
vmov.f32 d0, #-0.1875 | fmov v0.2s, #-0.1875
vmvn.i32 q0, #0x3400 | mvni v0.4s, #0x34, lsl #8
vmvn.i16 d0, #0x12 | mvni v0.4h, #0x12
vmvn.i32 q0, #0x12ffff | mvni v0.4s, #0x12, msl #16
vmvn.i32 d0, #0x7f | mvni v0.2s, #0x7f
vorr.i32 q0, #0x7f00 | orr v0.4s, #0x7f, lsl #8
vorr.i16 d0, #0x81 | orr v0.4h, #0x81
vorr.i32 d0, #0x5a000000 | orr v0.2s, #0x5a, lsl #24
vbic.i32 q0, #0x12000000 | bic v0.4s, #0x12, lsl #24
vbic.i16 q0, #0xab00 | bic v0.8h, #0xab, lsl #8
vbic.i32 d0, #0x00340000 | bic v0.2s, #0x34, lsl #16
# ---- two registers, miscellaneous
vrev64.8 q0, q1 | rev64 v0.16b, v1.16b
vrev64.16 d0, d2 | rev64 v0.4h, v1.4h
vrev64.32 q0, q1 | rev64 v0.4s, v1.4s
vrev32.8 q0, q1 | rev32 v0.16b, v1.16b
vrev32.16 d0, d2 | rev32 v0.4h, v1.4h
vrev16.8 q0, q1 | rev16 v0.16b, v1.16b
vpaddl.s8 q0, q1 | saddlp v0.8h, v1.16b
vpaddl.u16 d0, d2 | uaddlp v0.2s, v1.4h
vpaddl.s32 q0, q1 | saddlp v0.2d, v1.4s
vpaddl.u8 d0, d2 | uaddlp v0.4h, v1.8b
vpadal.u8 q0, q1 | uadalp v0.8h, v1.16b
vpadal.s32 d0, d2 | sadalp v0.1d, v1.2s
vpadal.s16 q0, q1 | sadalp v0.4s, v1.8h
vcls.s8 q0, q1 | cls v0.16b, v1.16b
vcls.s16 d0, d2 | cls v0.4h, v1.4h
vcls.s32 q0, q1 | cls v0.4s, v1.4s
vclz.i8 q0, q1 | clz v0.16b, v1.16b
vclz.i16 q0, q1 | clz v0.8h, v1.8h
vclz.i32 d0, d2 | clz v0.2s, v1.2s
vcnt.8 q0, q1 | cnt v0.16b, v1.16b
vcnt.8 d0, d2 | cnt v0.8b, v1.8b
vmvn q0, q1 | mvn v0.16b, v1.16b
vqabs.s8 q0, q1 | sqabs v0.16b, v1.16b
vqabs.s32 d0, d2 | sqabs v0.2s, v1.2s
vqneg.s16 q0, q1 | sqneg v0.8h, v1.8h
vqneg.s8 d0, d2 | sqneg v0.8b, v1.8b
vcgt.s8 q0, q1, #0 | cmgt v0.16b, v1.16b, #0
vcge.s16 q0, q1, #0 | cmge v0.8h, v1.8h, #0
vceq.i32 q0, q1, #0 | cmeq v0.4s, v1.4s, #0
vcle.s8 d0, d2, #0 | cmle v0.8b, v1.8b, #0
vclt.s32 q0, q1, #0 | cmlt v0.4s, v1.4s, #0
vabs.s8 q0, q1 | abs v0.16b, v1.16b
vabs.s16 d0, d2 | abs v0.4h, v1.4h
vabs.s32 q0, q1 | abs v0.4s, v1.4s
vneg.s8 q0, q1 | neg v0.16b, v1.16b
vneg.s32 d0, d2 | neg v0.2s, v1.2s
vcgt.f32 q0, q1, #0 | fcmgt v0.4s, v1.4s, #0.0
vcge.f32 q0, q1, #0 | fcmge v0.4s, v1.4s, #0.0
vceq.f32 d0, d2, #0 | fcmeq v0.2s, v1.2s, #0.0
vcle.f32 q0, q1, #0 | fcmle v0.4s, v1.4s, #0.0
vclt.f32 q0, q1, #0 | fcmlt v0.4s, v1.4s, #0.0
vabs.f32 q0, q1 | fabs v0.4s, v1.4s
vneg.f32 d0, d2 | fneg v0.2s, v1.2s
vswp d0, d2 | mov v16.d[0], v0.d[0] ; mov v0.d[0], v1.d[0] ; mov v1.d[0], v16.d[0]
vswp q0, q1 | mov v16.16b, v0.16b ; mov v0.16b, v1.16b ; mov v1.16b, v16.16b
vtrn.8 d0, d2 | trn1 v16.8b, v0.8b, v1.8b ; trn2 v1.8b, v0.8b, v1.8b ; mov v0.16b, v16.16b
vtrn.16 q0, q1 | trn1 v16.8h, v0.8h, v1.8h ; trn2 v1.8h, v0.8h, v1.8h ; mov v0.16b, v16.16b
vtrn.32 d0, d2 | trn1 v16.2s, v0.2s, v1.2s ; trn2 v1.2s, v0.2s, v1.2s ; mov v0.16b, v16.16b
vtrn.32 q0, q1 | trn1 v16.4s, v0.4s, v1.4s ; trn2 v1.4s, v0.4s, v1.4s ; mov v0.16b, v16.16b
vuzp.8 d0, d2 | uzp1 v16.8b, v0.8b, v1.8b ; uzp2 v1.8b, v0.8b, v1.8b ; mov v0.16b, v16.16b
vuzp.16 q0, q1 | uzp1 v16.8h, v0.8h, v1.8h ; uzp2 v1.8h, v0.8h, v1.8h ; mov v0.16b, v16.16b
vuzp.32 q0, q1 | uzp1 v16.4s, v0.4s, v1.4s ; uzp2 v1.4s, v0.4s, v1.4s ; mov v0.16b, v16.16b
vzip.8 d0, d2 | zip1 v16.8b, v0.8b, v1.8b ; zip2 v1.8b, v0.8b, v1.8b ; mov v0.16b, v16.16b
vzip.16 d0, d2 | zip1 v16.4h, v0.4h, v1.4h ; zip2 v1.4h, v0.4h, v1.4h ; mov v0.16b, v16.16b
vzip.8 q0, q1 | zip1 v16.16b, v0.16b, v1.16b ; zip2 v1.16b, v0.16b, v1.16b ; mov v0.16b, v16.16b
vzip.32 q0, q1 | zip1 v16.4s, v0.4s, v1.4s ; zip2 v1.4s, v0.4s, v1.4s ; mov v0.16b, v16.16b
vmovn.i16 d0, q1 | xtn v0.8b, v1.8h
vmovn.i64 d0, q1 | xtn v0.2s, v1.2d
vmovn.i32 d1, q1 | xtn2 v0.8h, v1.4s
vqmovn.s16 d0, q1 | sqxtn v0.8b, v1.8h
vqmovn.u32 d0, q1 | uqxtn v0.4h, v1.4s
vqmovn.s64 d0, q1 | sqxtn v0.2s, v1.2d
vqmovun.s64 d0, q1 | sqxtun v0.2s, v1.2d
vqmovun.s16 d0, q1 | sqxtun v0.8b, v1.8h
vcvt.f16.f32 d0, q1 | fcvtn v0.4h, v1.4s
vcvt.f32.f16 q0, d2 | fcvtl v0.4s, v1.4h
vcvt.f32.f16 q0, d3 | fcvtl2 v0.4s, v1.8h
vrecpe.f32 q0, q1 | frecpe v0.4s, v1.4s
vrecpe.u32 q0, q1 | urecpe v0.4s, v1.4s
vrsqrte.f32 q0, q1 | frsqrte v0.4s, v1.4s
vrsqrte.u32 d0, d2 | ursqrte v0.2s, v1.2s
vcvt.s32.f32 q0, q1 | fcvtzs v0.4s, v1.4s
vcvt.u32.f32 q0, q1 | fcvtzu v0.4s, v1.4s
vcvt.f32.s32 q0, q1 | scvtf v0.4s, v1.4s
vcvt.f32.u32 d0, d2 | ucvtf v0.2s, v1.2s
# ---- VEXT, VTBL/VTBX, VDUP
vext.8 q0, q1, q2, #5 | ext v0.16b, v1.16b, v2.16b, #5
vext.8 d0, d2, d4, #3 | ext v0.8b, v1.8b, v2.8b, #3
vext.16 d0, d2, d4, #1 | ext v0.8b, v1.8b, v2.8b, #2
vext.32 q0, q1, q2, #3 | ext v0.16b, v1.16b, v2.16b, #12
vext.64 q0, q1, q2, #1 | ext v0.16b, v1.16b, v2.16b, #8
vtbl.8 d0, {d2}, d4 | movi v16.2d, #0 ; mov v16.d[0], v1.d[0] ; tbl v0.8b, {v16.16b}, v2.8b
vtbl.8 d0, {d2, d3}, d4 | tbl v0.8b, {v1.16b}, v2.8b
vtbl.8 d0, {d2, d3, d4}, d6 | mov v16.16b, v1.16b ; movi v17.2d, #0 ; mov v17.d[0], v2.d[0] ; tbl v0.8b, {v16.16b, v17.16b}, v3.8b
vtbl.8 d0, {d2, d3, d4, d5}, d6 | tbl v0.8b, {v1.16b, v2.16b}, v3.8b
vtbx.8 d0, {d2}, d4 | movi v17.8b, #8 ; cmhs v18.8b, v2.8b, v17.8b ; orr v18.8b, v2.8b, v18.8b ; mov v16.d[0], v1.d[0] ; tbx v0.8b, {v16.16b}, v18.8b
vtbx.8 d0, {d2, d3}, d4 | tbx v0.8b, {v1.16b}, v2.8b
vtbx.8 d0, {d2, d3, d4, d5}, d6 | tbx v0.8b, {v1.16b, v2.16b}, v3.8b
vdup.8 q0, d2[5] | dup v0.16b, v1.b[5]
vdup.16 d0, d3[1] | dup v0.4h, v1.h[5]
vdup.32 q0, d5[1] | dup v0.4s, v2.s[3]
# ---- scalar transfers (coprocessor space, vfp.rs)
vmov.8 d0[5], r1 | ins v0.b[5], w1
vmov.16 d1[2], r1 | ins v0.h[6], w1
vmov.32 d2[1], r1 | ins v1.s[1], w1
vmov.s8 r0, d2[7] | smov w0, v1.b[7]
vmov.u8 r0, d3[1] | umov w0, v1.b[9]
vmov.s16 r0, d2[3] | smov w0, v1.h[3]
vmov.u16 r0, d3[0] | umov w0, v1.h[4]
vmov.32 r0, d3[1] | mov w0, v1.s[3]
vdup.8 q0, r1 | dup v0.16b, w1
vdup.16 d0, r1 | dup v0.4h, w1
vdup.32 q1, r2 | dup v1.4s, w2
# ---- odd D registers
vadd.i8 d1, d3, d5 | ext v16.16b, v1.16b, v1.16b, #8 ; ext v17.16b, v2.16b, v2.16b, #8 ; add v16.8b, v16.8b, v17.8b ; mov v0.d[1], v16.d[0]
vqadd.s16 d3, d1, d7 | ext v16.16b, v0.16b, v0.16b, #8 ; ext v17.16b, v3.16b, v3.16b, #8 ; sqadd v16.4h, v16.4h, v17.4h ; mov v1.d[1], v16.d[0]
vext.8 d1, d3, d2, #6 | ext v16.16b, v1.16b, v1.16b, #8 ; ext v16.8b, v16.8b, v1.8b, #6 ; mov v0.d[1], v16.d[0]
vfma.f32 d1, d3, d31 | ext v16.16b, v1.16b, v1.16b, #8 ; ext v17.16b, v15.16b, v15.16b, #8 ; ext v18.16b, v0.16b, v0.16b, #8 ; fmla v18.2s, v16.2s, v17.2s ; mov v0.d[1], v18.d[0]
# ---- element and structure loads/stores
vld1.8 {d0}, [r0] | ld1 {v0.8b}, [x0]
vld1.16 {d0, d1}, [r0]! | ld1 {v0.8h}, [x0], #16
vld1.32 {d0, d1, d2}, [r0], r1 | ld1 {v0.4s}, [x0] ; ldr d1, [x0, #16] ; add x0, x0, x1
vld1.64 {d0, d1, d2, d3}, [r0:256]! | ld1 {v0.2d, v1.2d}, [x0], #32
vld1.8 {d1, d2, d3, d4}, [r0:128] | ld1 {v16.16b, v17.16b}, [x0] ; mov v0.d[1], v16.d[0] ; mov v1.d[0], v16.d[1] ; mov v1.d[1], v17.d[0] ; mov v2.d[0], v17.d[1]
vld2.8 {d0, d1}, [r0] | ld2 {v16.8b, v17.8b}, [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0]
vld2.16 {d0, d2}, [r0]! | ld2 {v16.4h, v17.4h}, [x0], #16 ; mov v0.d[0], v16.d[0] ; mov v1.d[0], v17.d[0]
vld2.32 {d0, d1, d2, d3}, [r0:128] | ld2 {v0.4s, v1.4s}, [x0]
vld2.8 {d4, d5, d6, d7}, [r0], r1 | ld2 {v2.16b, v3.16b}, [x0], x1
vld3.8 {d0, d1, d2}, [r0] | ld3 {v16.8b, v17.8b, v18.8b}, [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0]
vld3.16 {d0, d2, d4}, [r0:64]! | ld3 {v16.4h, v17.4h, v18.4h}, [x0], #24 ; mov v0.d[0], v16.d[0] ; mov v1.d[0], v17.d[0] ; mov v2.d[0], v18.d[0]
vld3.32 {d1, d2, d3}, [r0] | ld3 {v16.2s, v17.2s, v18.2s}, [x0] ; mov v0.d[1], v16.d[0] ; mov v1.d[0], v17.d[0] ; mov v1.d[1], v18.d[0]
vld4.8 {d0, d1, d2, d3}, [r0] | ld4 {v16.8b, v17.8b, v18.8b, v19.8b}, [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0] ; mov v1.d[1], v19.d[0]
vld4.32 {d0, d2, d4, d6}, [r0:128] | ld4 {v16.2s, v17.2s, v18.2s, v19.2s}, [x0] ; mov v0.d[0], v16.d[0] ; mov v1.d[0], v17.d[0] ; mov v2.d[0], v18.d[0] ; mov v3.d[0], v19.d[0]
vld4.16 {d0, d1, d2, d3}, [r0:256]! | ld4 {v16.4h, v17.4h, v18.4h, v19.4h}, [x0], #32 ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0] ; mov v1.d[1], v19.d[0]
vld1.8 {d0[3]}, [r0] | ld1 {v0.b}[3], [x0]
vld1.16 {d1[2]}, [r0]! | ld1 {v0.h}[6], [x0], #2
vld1.32 {d2[1]}, [r0:32], r1 | ld1 {v1.s}[1], [x0], x1
vld1.16 {d4[3]}, [r0:16] | ld1 {v2.h}[3], [x0]
vld2.8 {d0[1], d1[1]}, [r0] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; ld2 {v16.b, v17.b}[1], [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0]
vld2.16 {d0[2], d2[2]}, [r0:32]! | mov v16.d[0], v0.d[0] ; mov v17.d[0], v1.d[0] ; ld2 {v16.h, v17.h}[2], [x0], #4 ; mov v0.d[0], v16.d[0] ; mov v1.d[0], v17.d[0]
vld2.32 {d0[1], d1[1]}, [r0:64] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; ld2 {v16.s, v17.s}[1], [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0]
vld3.32 {d0[1], d1[1], d2[1]}, [r0] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; mov v18.d[0], v1.d[0] ; ld3 {v16.s, v17.s, v18.s}[1], [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0]
vld3.8 {d0[6], d1[6], d2[6]}, [r0]! | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; mov v18.d[0], v1.d[0] ; ld3 {v16.b, v17.b, v18.b}[6], [x0], #3 ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0]
vld4.8 {d0[7], d1[7], d2[7], d3[7]}, [r0:32] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; mov v18.d[0], v1.d[0] ; mov v19.d[0], v1.d[1] ; ld4 {v16.b, v17.b, v18.b, v19.b}[7], [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0] ; mov v1.d[1], v19.d[0]
vld4.16 {d0[1], d2[1], d4[1], d6[1]}, [r0:64]! | mov v16.d[0], v0.d[0] ; mov v17.d[0], v1.d[0] ; mov v18.d[0], v2.d[0] ; mov v19.d[0], v3.d[0] ; ld4 {v16.h, v17.h, v18.h, v19.h}[1], [x0], #8 ; mov v0.d[0], v16.d[0] ; mov v1.d[0], v17.d[0] ; mov v2.d[0], v18.d[0] ; mov v3.d[0], v19.d[0]
vld4.32 {d0[0], d1[0], d2[0], d3[0]}, [r0:128] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; mov v18.d[0], v1.d[0] ; mov v19.d[0], v1.d[1] ; ld4 {v16.s, v17.s, v18.s, v19.s}[0], [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0] ; mov v1.d[1], v19.d[0]
vld1.8 {d0[]}, [r0] | ld1r {v0.8b}, [x0]
vld1.16 {d0[], d1[]}, [r0:16]! | ld1r {v0.8h}, [x0], #2
vld1.32 {d0[]}, [r0], r1 | ld1r {v0.2s}, [x0], x1
vld2.8 {d0[], d1[]}, [r0] | ld2r {v16.8b, v17.8b}, [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0]
vld2.32 {d0[], d2[]}, [r0:64] | ld2r {v16.2s, v17.2s}, [x0] ; mov v0.d[0], v16.d[0] ; mov v1.d[0], v17.d[0]
vld3.16 {d0[], d1[], d2[]}, [r0]! | ld3r {v16.4h, v17.4h, v18.4h}, [x0], #6 ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0]
vld4.8 {d0[], d1[], d2[], d3[]}, [r0:32] | ld4r {v16.8b, v17.8b, v18.8b, v19.8b}, [x0] ; mov v0.d[0], v16.d[0] ; mov v0.d[1], v17.d[0] ; mov v1.d[0], v18.d[0] ; mov v1.d[1], v19.d[0]
vld4.32 {d0[], d2[], d4[], d6[]}, [r0:128] | ld4r {v16.2s, v17.2s, v18.2s, v19.2s}, [x0] ; mov v0.d[0], v16.d[0] ; mov v1.d[0], v17.d[0] ; mov v2.d[0], v18.d[0] ; mov v3.d[0], v19.d[0]
vst1.8 {d0}, [r0] | st1 {v0.8b}, [x0]
vst1.32 {d0, d1, d2, d3}, [r0]! | st1 {v0.4s, v1.4s}, [x0], #32
vst1.16 {d2, d3, d4}, [r0:64], r1 | st1 {v1.8h}, [x0] ; str d2, [x0, #16] ; add x0, x0, x1
vst1.64 {d1, d2}, [r0:128] | ext v16.16b, v0.16b, v1.16b, #8 ; st1 {v16.2d}, [x0]
vst2.8 {d0, d1}, [r0] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; st2 {v16.8b, v17.8b}, [x0]
vst2.16 {d0, d1, d2, d3}, [r0]! | st2 {v0.8h, v1.8h}, [x0], #32
vst2.32 {d0, d2}, [r0:128] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v1.d[0] ; st2 {v16.2s, v17.2s}, [x0]
vst3.16 {d0, d2, d4}, [r0] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v1.d[0] ; mov v18.d[0], v2.d[0] ; st3 {v16.4h, v17.4h, v18.4h}, [x0]
vst3.8 {d0, d1, d2}, [r0]! | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; mov v18.d[0], v1.d[0] ; st3 {v16.8b, v17.8b, v18.8b}, [x0], #24
vst4.32 {d0, d1, d2, d3}, [r0] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; mov v18.d[0], v1.d[0] ; mov v19.d[0], v1.d[1] ; st4 {v16.2s, v17.2s, v18.2s, v19.2s}, [x0]
vst4.8 {d0, d2, d4, d6}, [r0:64], r1 | mov v16.d[0], v0.d[0] ; mov v17.d[0], v1.d[0] ; mov v18.d[0], v2.d[0] ; mov v19.d[0], v3.d[0] ; st4 {v16.8b, v17.8b, v18.8b, v19.8b}, [x0], x1
vst1.8 {d0[5]}, [r0] | st1 {v0.b}[5], [x0]
vst1.32 {d3[1]}, [r0:32]! | st1 {v1.s}[3], [x0], #4
vst2.16 {d0[1], d1[1]}, [r0] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; st2 {v16.h, v17.h}[1], [x0]
vst3.32 {d0[1], d2[1], d4[1]}, [r0], r1 | mov v16.d[0], v0.d[0] ; mov v17.d[0], v1.d[0] ; mov v18.d[0], v2.d[0] ; st3 {v16.s, v17.s, v18.s}[1], [x0], x1
vst4.8 {d0[2], d1[2], d2[2], d3[2]}, [r0:32]! | mov v16.d[0], v0.d[0] ; mov v17.d[0], v0.d[1] ; mov v18.d[0], v1.d[0] ; mov v19.d[0], v1.d[1] ; st4 {v16.b, v17.b, v18.b, v19.b}[2], [x0], #4
vst4.16 {d0[3], d2[3], d4[3], d6[3]}, [r0:64] | mov v16.d[0], v0.d[0] ; mov v17.d[0], v1.d[0] ; mov v18.d[0], v2.d[0] ; mov v19.d[0], v3.d[0] ; st4 {v16.h, v17.h, v18.h, v19.h}[3], [x0]
"""


def assemble(line, thumb):
    with tempfile.TemporaryDirectory() as tmp:
        src = os.path.join(tmp, "t.s")
        obj = os.path.join(tmp, "t.o")
        binf = os.path.join(tmp, "t.bin")
        with open(src, "w") as f:
            f.write(".syntax unified\n.fpu neon-vfpv4\n%s\n%s\n" % (".thumb" if thumb else ".arm", line))
        subprocess.run(["arm-none-eabi-as", "-march=armv7ve", "-mfpu=neon-vfpv4", "-o", obj, src], check=True)
        subprocess.run(["arm-none-eabi-objcopy", "-O", "binary", obj, binf], check=True)
        data = open(binf, "rb").read()
    assert len(data) == 4, line
    if thumb:
        return int.from_bytes(data[0:2], "little") << 16 | int.from_bytes(data[2:4], "little")
    return int.from_bytes(data, "little")


def dregs(text):
    """D registers named in an operand (q<n> = two)."""
    regs = []
    text = re.sub(r"d(\d+)-d(\d+)", lambda m: ",".join("d%d" % i for i in range(int(m.group(1)), int(m.group(2)) + 1)), text)
    for kind, num in re.findall(r"\b([dq])(\d+)", text):
        n = int(num)
        regs += [2 * n, 2 * n + 1] if kind == "q" else [n]
    return regs


def operands(text):
    parts, depth, cur = [], 0, ""
    for ch in text:
        if ch in "{[":
            depth += 1
        elif ch in "}]":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur.strip())
            cur = ""
        else:
            cur += ch
    parts.append(cur.strip())
    return parts


def destinations(a32, opts):
    mnemonic, _, rest = a32.partition(" ")
    ops = operands(rest)
    dmask, rmask = 0, 0
    explicit = [o[5:] for o in opts if o.startswith("dest=")]
    if explicit:
        for reg in explicit[0].split(","):
            if reg[0] == "r":
                rmask |= 1 << int(reg[1:])
            else:
                for d in dregs(reg):
                    dmask |= 1 << d
        return dmask, rmask
    base = mnemonic.split(".")[0]
    if base.startswith("vst"):
        written = []
    elif base.startswith("vld"):
        written = dregs(ops[0])
    elif base in ("vswp", "vtrn", "vuzp", "vzip"):
        written = dregs(ops[0]) + dregs(ops[1])
    elif ops[0].startswith("r"):
        rmask |= 1 << int(ops[0][1:])
        written = []
    else:
        written = dregs(ops[0])
    for d in written:
        dmask |= 1 << d
    if "[r0" in rest and ("]!" in rest or re.search(r"\], r\d", rest)):
        rmask |= 1
    return dmask, rmask


def main():
    out = sys.stdout
    out.write("// Generated by tools/gen_neon_cases.py with arm-none-eabi-as; do not edit.\n")
    out.write("// (A32 text, A32 word, T32 word, D registers written, r0-r7 written, memory form,\n")
    out.write("//  inputs without infinities, AArch64 host equivalent)\n")
    out.write("neon_cases! {\n")
    count = 0
    for line in CASES.strip().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        fields = [f.strip() for f in line.split("|")]
        a32, host = fields[0], fields[1]
        opts = fields[2].split() if len(fields) > 2 else []
        arm = assemble(a32, False)
        thumb = assemble(a32, True)
        dmask, rmask = destinations(a32, opts)
        memory = "[r0" in a32
        # asm! templates: braces are doubled
        host_lines = "\\n".join(h.strip() for h in host.split(";")).replace("{", "{{").replace("}", "}}")
        out.write('    ("%s", 0x%08x, 0x%08x, 0x%08x, 0x%02x, %s, %s, "%s"),\n' % (
            a32, arm, thumb, dmask, rmask, "true" if memory else "false",
            "true" if "noinf" in opts else "false", host_lines))
        count += 1
    out.write("}\n")
    sys.stderr.write("%d cases\n" % count)


if __name__ == "__main__":
    main()
