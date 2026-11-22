#!/bin/sh
# Guest side of the Jetson Nano's GL tests (Debian 13 with gcc, libegl-dev and libgles-dev): build the
# three test programs, render on the GPU and in software, compare.
H=${GPU_SERVER:-http://10.0.2.2:8800}
cd /tmp && rm -rf gn && mkdir gn && cd gn
for t in gles2test gltest gles32test; do
    python3 -c "import urllib.request as u; open('$t.c','wb').write(u.urlopen('$H/$t.c').read())" || exit 1
    gcc -O1 -o $t $t.c -lEGL -lGLESv2 -lm || exit 1
    mkdir gpu-$t sw-$t
    ./$t run gpu-$t 2>&1 | grep -v 'done$'
    driver=llvmpipe
    [ $t = gles2test ] && driver=softpipe
    GALLIUM_DRIVER=$driver LIBGL_ALWAYS_SOFTWARE=1 ./$t run sw-$t >sw.log 2>&1 || tail -2 sw.log
    ./$t cmp gpu-$t sw-$t
done
echo NANO-GL-DONE
