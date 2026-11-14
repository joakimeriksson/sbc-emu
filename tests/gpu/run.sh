#!/bin/sh
# Guest side of the GPU tests: build, render on V3D and on llvmpipe, compare, upload.
H=http://10.0.2.2:8800
cd /tmp && rm -rf gt && mkdir -p gt/v3d gt/sw && cd gt
wget -q $H/gltest.c $H/put.py || { echo "fetch failed"; exit 1; }
gcc -O1 -o gltest gltest.c -lEGL -lGLESv2 -lm || exit 1
./gltest run v3d $1 2>&1 | tail -40
LIBGL_ALWAYS_SOFTWARE=1 ./gltest run sw $1 > sw.log 2>&1; tail -3 sw.log
./gltest cmp v3d sw | tee cmp.txt
tar cf results.tar v3d/*.ppm cmp.txt
python3 put.py $H results.tar cmp.txt
echo GLTEST-DONE
