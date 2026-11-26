# yolo-input.py IMAGE OUTDIR (on the host): YOLOv8n's 640x640 letterboxed input (OUTDIR/input.bin,
# float32 NCHW), its scale (OUTDIR/input.scale), and the reference detections from ONNX Runtime
# with OUTDIR/yolov8n.onnx, which tests/gpu/l4t/trtpost.py's output on the Jetson should match.
#   uv run --with opencv-python-headless --with onnxruntime python tests/gpu/l4t/yolo-input.py cache/l4t/bus.jpg cache/l4t/serve
import os, sys
import cv2
import numpy as np
sys.dont_write_bytecode = True  # (no __pycache__ in the repository)
sys.path.insert(0, os.path.dirname(__file__))
from trtpost import detections, NAMES

img, out = cv2.imread(sys.argv[1]), sys.argv[2]
h, w = img.shape[:2]
s = 640 / max(h, w)
pad = np.full((640, 640, 3), 114, np.uint8)
pad[:round(h * s), :round(w * s)] = cv2.resize(img, (round(w * s), round(h * s)))
blob = cv2.dnn.blobFromImage(pad, 1 / 255.0, (640, 640), swapRB=True).astype(np.float32)
blob.tofile(os.path.join(out, 'input.bin'))
open(os.path.join(out, 'input.scale'), 'w').write(f'{s}\n')
import onnxruntime as ort
sess = ort.InferenceSession(os.path.join(out, 'yolov8n.onnx'))
for c, conf, (x, y, bw, bh) in detections(sess.run(None, {'images': blob})[0], s):
    print(f'DET {NAMES.get(c, c)} {conf:.2f} [{x:.0f},{y:.0f},{bw:.0f},{bh:.0f}]')
