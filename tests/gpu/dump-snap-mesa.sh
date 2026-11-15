#!/bin/sh
# Dump the QPU code the snap Mesa (mesa-2404) compiles for one GL scene's fragment shaders.
H=http://10.0.2.2:8800
M=/snap/mesa-2404/current/usr
cd /tmp && rm -rf gd && mkdir -p gd/v3d && cd gd
for f in gltest put.py; do wget -q $H/$f || { echo "fetch $f failed"; exit 1; }; done
chmod +x gltest
export LD_LIBRARY_PATH=$M/lib/aarch64-linux-gnu __EGL_VENDOR_LIBRARY_DIRS=$M/share/glvnd/egl_vendor.d LIBGL_DRIVERS_PATH=$M/lib/aarch64-linux-gnu/dri
V3D_DEBUG=qpu,fs ./gltest run v3d $1 > fs-qpu.txt 2>&1
python3 put.py $H fs-qpu.txt
echo DUMP-DONE
