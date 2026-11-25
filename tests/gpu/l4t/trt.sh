# Under L4T (scripts/jetson-nano-tensorrt.txt): YOLOv8n through TensorRT, the output decoded.
. /opt/l4t/env.sh
/opt/l4t/usr/src/tensorrt/bin/trtexec --onnx=/opt/l4t/y/yolov8n.onnx --workspace=512 \
  --loadInputs=images:/opt/l4t/y/input.bin --exportOutput=/tmp/out.json \
  --iterations=1 --warmUp=0 --duration=0 --avgRuns=1 --minTiming=1 --avgTiming=1 \
  --tacticSources=-CUDNN,-CUBLAS,-CUBLAS_LT 2>&1 | grep -E "Engine built|GPU Compute Time: min|PASSED|FAILED|\[E\]|rror|not found"
python3 /opt/l4t/trtpost.py /tmp/out.json
echo TRT-""END
