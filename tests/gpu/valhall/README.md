# Valhall shader fixtures

`triangle.asm` is assembled using Mesa's Valhall `asm.py` and `ISA.xml` from commit
`06efd8542f442e701685e26abe4d73c4ddf7b249` (under
`src/panfrost/compiler/bifrost/valhall`). `triangle.json` contains its little-endian
64-bit instruction words, reproduced in `panthor-shader.py` so the guest needs no
external assembler. Shader unit tests use the same Mesa-assembled words.

The shader receives output address, coordinates, edge coefficients and background
color through FAU uniforms. It calculates barycentric coverage and color itself.
The guest submits one single-invocation compute dispatch per pixel; this exercises
actual Valhall shader execution rather than the fixed-function triangle pipeline.

`workgroup-triangle.asm` / `.json` use the same Mesa assembler revision. This
variant obtains coordinates and output addresses from preloaded global IDs and
runs all pixels in one compute dispatch. `panthor-shader.py` embeds these words.
The original triangle fixture remains a scalar instruction regression test.

`split-registers.asm` / `.json` use the same assembler revision to exercise
R48-R63 as both scalar registers and vector staging registers. `workgroup-triangle-32.asm`
/ `.json` move the triangle shader's r16/r17 temporaries to r48/r49 so it runs
with 32-register allocation. Both allocation modes use the same preload IDs,
framebuffer and pixel oracle in `panthor-shader.py`.

`attribute-load.asm` / `.json` use the same Mesa revision and its native
`LD_ATTR_IMM.v2.f32` encoding. Linux test `panthor-attributes.py` binds an attribute
table and a vertex-buffer table, loads three vertices, and writes their XY/0/1
vectors to a separate BO region. It also checks 12 instance values with a divisor-four descriptor, verifying all
vectors, surrounding sentinels and DRM fences. This tests attribute execution,
not triangle rasterization.

`position.asm` / `.json` exactly reproduce the seven instruction words captured
from the real Mesa GLES position shader: attribute load, viewport FMA operations,
position-buffer address and an internally streaming position store. The unit
test supplies hardware position table 13 using GPU-mapped storage.
`position-compute.asm` / `.json` add an internal-ID move and replace table 13 with
ordinary structured table 3; `panthor-position.py` runs that adapted shader
through Linux compute and checks all transformed XY/Z/W vectors and its fence.
Neither test establishes `RUN_IDVS`, tiling or fragment/rasterizer support.
