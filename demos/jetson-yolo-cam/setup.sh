#!/bin/sh
# Once, in Debian on the Jetson Nano's card (--sd-persist): what yolo_cam.py needs. The host
# serves this folder and the YOLOv8n model (docs/jetson-nano.md#webcam) at $DEMO_SERVER.
set -eu
H=${DEMO_SERVER:-http://10.0.2.2:8800}
get() {
    python3 -c "import urllib.request as u,sys; d=u.urlopen(sys.argv[1]).read(); open(sys.argv[2]+'.part','wb').write(d)" "$H/$1" "$2"
    mv "$2.part" "$2"
}
export DEBIAN_FRONTEND=noninteractive
apt-get -qq update
# the root fills the card (made larger on the host); growpart fails harmlessly when it does already
apt-get -y -qq install --no-install-recommends cloud-guest-utils >/dev/null
growpart /dev/mmcblk0 1 >/dev/null || true
resize2fs /dev/mmcblk0p1 >/dev/null 2>&1 || true
free_mb=$(df -m / | awk 'NR == 2 { print $4 }')
if [ "$free_mb" -lt 2500 ]; then
    echo "only $free_mb MB free on the card: make it larger on the host (truncate -s 6G)" >&2
    exit 1
fi
apt-get -y -qq install --no-install-recommends python3-opencv python3-numpy python3-venv python3-pip \
    mesa-vulkan-drivers mesa-opencl-icd libvulkan1 libvulkan-dev gcc libc6-dev >/dev/null
D=/opt/yolo-cam
mkdir -p $D/model
for f in yolo_cam.py run.sh device_local_layer.c device_local_layer.json; do get $f $D/$f; done
for f in model.ncnn.param model.ncnn.bin yolov8n.onnx; do get model/$f $D/model/$f; done
# NVK on Tegra has no device-local memory type, which ncnn needs: a layer marks one
gcc -O2 -shared -fPIC -o $D/libdevice_local_layer.so $D/device_local_layer.c
[ -x $D/venv/bin/python ] || python3 -m venv --system-site-packages $D/venv
$D/venv/bin/pip -q install ncnn
echo SETUP_""OK
