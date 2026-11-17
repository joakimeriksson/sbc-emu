#!/bin/sh
# Guest side of the GLES 2 tests on the Mali-T760 (no network on the RK3288): build
# /root/gles2test.c, render every scene on the GPU and on softpipe, compare per pixel on
# the console. Results stay in /root/g2. gcc does not run on the RK3288 yet (the gcc
# driver finds none of its programs there), so scripts/mali-t760-fixture.sh builds
# gles2test on the emulated Orange Pi One. Usage: sh /root/mali-t760-gles2.sh [SCENE]
cd /root && mkdir -p g2/gpu g2/sw && cd g2
[ -x gles2test ] || gcc -O1 -o gles2test /root/gles2test.c -lEGL -lGLESv2 -lm || exit 1
./gles2test run gpu $1 2>&1 | tail -40
# the softpipe reference only depends on the scenes: keep it once made
[ -f sw/done ] && [ -z "$1" ] || { GALLIUM_DRIVER=softpipe LIBGL_ALWAYS_SOFTWARE=1 ./gles2test run sw $1 > sw.log 2>&1; tail -3 sw.log; [ -n "$1" ] || touch sw/done; }
./gles2test cmp gpu sw
