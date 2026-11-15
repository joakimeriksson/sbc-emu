#!/bin/sh
# The GL scenes under the Mesa that Ubuntu's snaps (Firefox) use, mesa-2404, on V3D and on its
# llvmpipe. Needs the gltest binary built elsewhere (build.sh) on the server.
H=http://10.0.2.2:8800
M=/snap/mesa-2404/current/usr
cd /tmp && rm -rf gs && mkdir -p gs/v3d gs/sw && cd gs
for f in gltest put.py cmp.py; do wget -q $H/$f || { echo "fetch $f failed"; exit 1; }; done
chmod +x gltest
export LD_LIBRARY_PATH=$M/lib/aarch64-linux-gnu __EGL_VENDOR_LIBRARY_DIRS=$M/share/glvnd/egl_vendor.d LIBGL_DRIVERS_PATH=$M/lib/aarch64-linux-gnu/dri
./gltest run v3d $1 2>&1 | tail -40
LIBGL_ALWAYS_SOFTWARE=1 ./gltest run sw $1 > sw.log 2>&1; head -1 sw.log; tail -2 sw.log
python3 cmp.py v3d sw | tee cmp-snap.txt
tar cf results-snap.tar v3d/*.ppm cmp-snap.txt
python3 put.py $H results-snap.tar cmp-snap.txt
echo GLTEST-DONE
