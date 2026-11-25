#!/bin/sh
# On the host, once: YOLOv8n exported for ncnn and as ONNX (Ultralytics, through uv), a test video
# from two sample pictures, and this folder: all in OUT, which the guest's setup.sh fetches from.
# Usage: demos/jetson-yolo-cam/host-setup.sh [OUT]   (default cache/yolo-cam)
set -eu
OUT=${1:-cache/yolo-cam}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT/model"
cp "$HERE"/yolo_cam.py "$HERE"/run.sh "$HERE"/setup.sh "$HERE"/device_local_layer.c "$HERE"/device_local_layer.json "$OUT/"
if [ ! -f "$OUT/model/model.ncnn.bin" ]; then
    (cd "$OUT/model" && uv run -q --python 3.12 --with ultralytics --with onnx --with onnxslim --with ncnn --with pnnx \
        sh -c 'yolo export model=yolov8n.pt format=onnx imgsz=640 opset=12 >/dev/null &&
               yolo export model=yolov8n.pt format=ncnn imgsz=640 >/dev/null &&
               mv yolov8n_ncnn_model/model.ncnn.param yolov8n_ncnn_model/model.ncnn.bin . && rm -rf yolov8n_ncnn_model')
fi
if [ ! -f "$OUT/street.mp4" ]; then
    curl -fsSL -o "$OUT/bus.jpg" https://ultralytics.com/images/bus.jpg
    curl -fsSL -o "$OUT/zidane.jpg" https://ultralytics.com/images/zidane.jpg
    ffmpeg -hide_banner -loglevel error -y -loop 1 -t 8 -i "$OUT/bus.jpg" -loop 1 -t 8 -i "$OUT/zidane.jpg" -filter_complex \
        "[0]scale=640:-2,crop=640:480:0:'(ih-480)*t/8',setsar=1[a];[1]scale=-2:480,crop=640:480:'(iw-640)*t/8':0,setsar=1[b];[a][b]concat=n=2:v=1[v]" \
        -map "[v]" -r 30 -pix_fmt yuv420p "$OUT/street.mp4"
fi
ls "$OUT" "$OUT/model"
