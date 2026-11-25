# trtpost.py OUT.json [SCALE]: YOLOv8n's output (trtexec --exportOutput, 1x84x8400) to detections in
# the original image (SCALE: the letterbox's, from yolo-input.py; default /opt/l4t/y/input.scale).
# numpy only; prints "DET class confidence [x,y,w,h]" as tests/gpu/l4t/yolo-input.py's reference does.
import json, sys
import numpy as np
NAMES = {0: 'person', 2: 'car', 5: 'bus', 7: 'truck', 11: 'stop sign'}


def nms(boxes, scores, iou=0.45):
    order, keep = list(np.argsort(-scores)), []
    while order:
        i = order.pop(0)
        keep.append(i)
        x1, y1, w1, h1 = boxes[i]
        rest = []
        for j in order:
            x2, y2, w2, h2 = boxes[j]
            iw = max(0.0, min(x1 + w1, x2 + w2) - max(x1, x2))
            ih = max(0.0, min(y1 + h1, y2 + h2) - max(y1, y2))
            inter = iw * ih
            if inter / (w1 * h1 + w2 * h2 - inter + 1e-9) <= iou:
                rest.append(j)
        order = rest
    return keep


def detections(p, s, conf_min=0.25):
    p = p.reshape(84, -1).T
    cls, conf = p[:, 4:].argmax(1), p[:, 4:].max(1)
    k = conf > conf_min
    boxes = np.array([[(x - w / 2) / s, (y - h / 2) / s, w / s, h / s] for x, y, w, h in p[k, :4]]).reshape(-1, 4)
    return [(int(cls[k][i]), float(conf[k][i]), boxes[i]) for i in nms(boxes, conf[k])]


if __name__ == '__main__':
    d = json.load(open(sys.argv[1]))
    t = d[0] if isinstance(d, list) else d
    s = float(sys.argv[2]) if len(sys.argv) > 2 else float(open('/opt/l4t/y/input.scale').read())
    for c, conf, (x, y, w, h) in detections(np.array(t['values'], np.float32), s):
        print(f'DET {NAMES.get(c, c)} {conf:.2f} [{x:.0f},{y:.0f},{w:.0f},{h:.0f}]')
