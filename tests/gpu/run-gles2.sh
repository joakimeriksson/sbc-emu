#!/bin/sh
# Guest side of the GLES 2 tests (GPUs without ES 3, the Orange Pi One's Mali-400): build
# gles2test, render on the GPU and on softpipe, compare per pixel, upload the results.
H=${GPU_SERVER:-http://10.0.2.2:8800}
cd /tmp && rm -rf g2 && mkdir -p g2/gpu g2/sw && cd g2
for f in gles2test.c put.py; do wget -q $H/$f || { echo "fetch $f failed"; exit 1; }; done
gcc -O1 -o gles2test gles2test.c -lEGL -lGLESv2 -lm || exit 1
./gles2test run gpu $1 2>&1 | tail -40
GALLIUM_DRIVER=softpipe LIBGL_ALWAYS_SOFTWARE=1 ./gles2test run sw $1 > sw.log 2>&1; tail -3 sw.log
./gles2test cmp gpu sw | tee cmp.txt
tar cf results.tar gpu/*.ppm cmp.txt
python3 put.py $H results.tar cmp.txt
echo GLES2TEST-DONE
