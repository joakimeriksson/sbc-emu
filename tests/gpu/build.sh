#!/bin/sh
# Build the GL test program in a guest that has a compiler and upload it (for guests without one).
H=http://10.0.2.2:8800
cd /tmp && rm -rf gb && mkdir gb && cd gb && wget -q $H/gltest.c $H/put.py && gcc -O1 -o gltest gltest.c -lEGL -lGLESv2 -lm && python3 put.py $H gltest && echo BUILD-OK
