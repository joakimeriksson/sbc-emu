# Under L4T (scripts/jetson-nano-cuda.txt): the CUDA driver API test, a PTX vector add.
. /opt/l4t/env.sh
ls /dev/nvhost-gpu /dev/nvhost-ctrl-gpu /dev/nvhost-as-gpu
/opt/l4t/cuda-vadd; echo CUDA_RC=$?
echo CUDA-""END
