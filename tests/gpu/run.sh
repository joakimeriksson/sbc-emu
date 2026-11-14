#!/bin/sh
# Guest side of the GPU tests: build, render on V3D and on the software renderers
# (llvmpipe for GL, lavapipe for Vulkan), compare per pixel, upload the results.
H=http://10.0.2.2:8800
cd /tmp && rm -rf gt && mkdir -p gt/v3d gt/sw && cd gt
for f in gltest.c put.py cmp.py vktest.c vk_col.vert vk_col.frag vk_tex.vert vk_tex.frag vk_comp.comp; do wget -q $H/$f || { echo "fetch $f failed"; exit 1; }; done
gcc -O1 -o gltest gltest.c -lEGL -lGLESv2 -lm || exit 1
./gltest run v3d $1 2>&1 | tail -40
LIBGL_ALWAYS_SOFTWARE=1 ./gltest run sw $1 > sw.log 2>&1; tail -3 sw.log
if command -v glslangValidator > /dev/null; then
  for s in vk_col.vert vk_col.frag vk_tex.vert vk_tex.frag vk_comp.comp; do glslangValidator -V -o $s.spv $s > /dev/null || echo "glslang $s failed"; done
  gcc -O1 -o vktest vktest.c -lvulkan && { ./vktest run V3D v3d 2>&1 | tail -10; ./vktest run llvmpipe sw > vksw.log 2>&1; tail -3 vksw.log; }
fi
python3 cmp.py v3d sw | tee cmp.txt
tar cf results.tar v3d/*.ppm cmp.txt
python3 put.py $H results.tar cmp.txt
echo GLTEST-DONE
