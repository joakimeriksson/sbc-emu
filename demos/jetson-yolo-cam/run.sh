#!/bin/sh
# yolo_cam.py with the backend's environment: run.sh nvk|cpu|opencl [yolo_cam.py's options]
D=/opt/yolo-cam
B=${1:-nvk}; [ $# -gt 0 ] && shift
case $B in
nvk) export NVK_I_WANT_A_BROKEN_VULKAN_DRIVER=1 VK_ADD_LAYER_PATH=$D VK_INSTANCE_LAYERS=VK_LAYER_SBC_uma ;;
opencl) export RUSTICL_ENABLE=nouveau OPENCV_OPENCL_DEVICE=rusticl:GPU:0 ;;
esac
exec $D/venv/bin/python $D/yolo_cam.py --backend "$B" --model $D/model "$@"
