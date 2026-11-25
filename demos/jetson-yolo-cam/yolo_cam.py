#!/usr/bin/env python3
"""YOLOv8n on the emulated Jetson Nano's webcam (docs/jetson-nano.md#webcam): frames from
/dev/video0 (the emulated USB camera, fed with --camera-source), objects found by YOLOv8n,
boxes drawn on the HDMI screen (/dev/fb0) and, with --upload URL, each picture sent to the host.

  yolo_cam.py [--backend nvk|cpu|opencl] [--frames N] [--upload http://10.0.2.2:8800/] [--model DIR]

nvk: ncnn on Vulkan (Mesa's NVK on the emulated GM20B; needs NVK_I_WANT_A_BROKEN_VULKAN_DRIVER=1
and a memory type marked device-local, see docs/jetson-nano-gpu.md); cpu: ncnn on the CPU;
opencl: OpenCV's DNN on Mesa's OpenCL (RUSTICL_ENABLE=nouveau). The model is YOLOv8n exported for
ncnn (DIR/model.ncnn.param, .bin) and, for OpenCL, as ONNX (DIR/yolov8n.onnx).
"""
import argparse
import mmap
import os
import time
import urllib.request

import cv2
import numpy as np

COCO = ('person bicycle car motorcycle airplane bus train truck boat traffic_light fire_hydrant stop_sign '
        'parking_meter bench bird cat dog horse sheep cow elephant bear zebra giraffe backpack umbrella handbag '
        'tie suitcase frisbee skis snowboard sports_ball kite baseball_bat baseball_glove skateboard surfboard '
        'tennis_racket bottle wine_glass cup fork knife spoon bowl banana apple sandwich orange broccoli carrot '
        'hot_dog pizza donut cake chair couch potted_plant bed dining_table toilet tv laptop mouse remote keyboard '
        'cell_phone microwave oven toaster sink refrigerator book clock vase scissors teddy_bear hair_drier '
        'toothbrush').split()


def letterbox(img):
    """The 640x640 network input: the picture scaled into it, grey around; and the scale."""
    h, w = img.shape[:2]
    s = 640 / max(h, w)
    pad = np.full((640, 640, 3), 114, np.uint8)
    pad[:round(h * s), :round(w * s)] = cv2.resize(img, (round(w * s), round(h * s)))
    return cv2.dnn.blobFromImage(pad, 1 / 255.0, (640, 640), swapRB=True), s


def detections(out, s, conf_min=0.3):
    """YOLOv8's 84x8400 output: (class, confidence, box x, y, w, h) in the picture, after NMS."""
    p = out.reshape(84, -1).T
    cls, conf = p[:, 4:].argmax(1), p[:, 4:].max(1)
    k = conf > conf_min
    boxes = [[(x - bw / 2) / s, (y - bh / 2) / s, bw / s, bh / s] for x, y, bw, bh in p[k, :4]]
    keep = cv2.dnn.NMSBoxes(boxes, conf[k].tolist(), conf_min, 0.45)
    return [(int(cls[k][i]), float(conf[k][i]), boxes[i]) for i in np.array(keep).flatten()]


class Model:
    def __init__(self, backend, model_dir):
        self.backend = backend
        if backend == 'opencl':
            self.net = cv2.dnn.readNetFromONNX(os.path.join(model_dir, 'yolov8n.onnx'))
            self.net.setPreferableBackend(cv2.dnn.DNN_BACKEND_OPENCV)
            self.net.setPreferableTarget(cv2.dnn.DNN_TARGET_OPENCL)
        else:
            import ncnn
            self.ncnn = ncnn
            self.net = ncnn.Net()
            self.net.opt.use_vulkan_compute = backend == 'nvk'
            self.net.load_param(os.path.join(model_dir, 'model.ncnn.param'))
            self.net.load_model(os.path.join(model_dir, 'model.ncnn.bin'))

    def __call__(self, blob):
        if self.backend == 'opencl':
            self.net.setInput(blob)
            return self.net.forward()
        ex = self.net.create_extractor()
        ex.input('in0', self.ncnn.Mat(np.ascontiguousarray(blob[0])))
        return np.array(ex.extract('out0')[1])


class Screen:
    """The HDMI framebuffer (XRGB8888): pictures in its middle, scaled up to fill its height."""

    def __init__(self):
        g = '/sys/class/graphics/fb0/'
        self.w, self.h = (int(v) for v in open(g + 'virtual_size').read().split(','))
        self.stride = int(open(g + 'stride').read())
        self.fd = os.open('/dev/fb0', os.O_RDWR)
        self.fb = mmap.mmap(self.fd, self.stride * self.h)
        self.fb[:] = bytes(len(self.fb))

    def show(self, img):
        h = self.h
        w = img.shape[1] * h // img.shape[0]
        px = cv2.cvtColor(cv2.resize(img, (w, h)), cv2.COLOR_BGR2BGRA)
        x0 = (self.w - w) // 2
        for y in range(h):
            o = y * self.stride + 4 * x0
            self.fb[o:o + 4 * w] = px[y].tobytes()


def main():
    a = argparse.ArgumentParser()
    a.add_argument('--backend', default='nvk', choices=['nvk', 'cpu', 'opencl'])
    a.add_argument('--frames', type=int, default=0, help='stop after N pictures (0: never)')
    a.add_argument('--device', default='/dev/video0')
    a.add_argument('--model', default='/opt/yolo-cam/model')
    a.add_argument('--upload', help='PUT each annotated picture to URL/frameNNNN.jpg')
    a.add_argument('--no-screen', action='store_true')
    o = a.parse_args()
    cap = cv2.VideoCapture(o.device, cv2.CAP_V4L2)
    if not cap.isOpened():
        raise SystemExit(f'{o.device}: no camera')
    model = Model(o.backend, o.model)
    screen = None if o.no_screen else Screen()
    n = 0
    while o.frames == 0 or n < o.frames:
        # the newest picture: drop the one the driver kept while the last was being looked at
        cap.grab()
        ok, img = cap.read()
        if not ok:
            break
        t = time.time()
        blob, s = letterbox(img)
        found = detections(model(blob), s)
        dt = time.time() - t
        for c, conf, (x, y, w, h) in found:
            p0, p1 = (int(x), int(y)), (int(x + w), int(y + h))
            cv2.rectangle(img, p0, p1, (0, 255, 0), 2)
            cv2.putText(img, f'{COCO[c]} {conf:.2f}', (p0[0], max(14, p0[1] - 5)), cv2.FONT_HERSHEY_SIMPLEX, 0.55, (0, 255, 0), 2)
        label = f'YOLOv8n {o.backend} on the emulated Jetson Nano  {dt:.2f} s/frame'
        hgt = img.shape[0]
        cv2.rectangle(img, (0, hgt - 22), (img.shape[1], hgt), (0, 0, 0), -1)
        cv2.putText(img, label, (6, hgt - 6), cv2.FONT_HERSHEY_SIMPLEX, 0.5, (255, 255, 255), 1)
        if screen:
            screen.show(img)
        if o.upload:
            ok, jpg = cv2.imencode('.jpg', img, [cv2.IMWRITE_JPEG_QUALITY, 85])
            urllib.request.urlopen(urllib.request.Request(f'{o.upload.rstrip("/")}/frame{n:04d}.jpg', data=jpg.tobytes(), method='PUT'))
        print(f'frame {n}: {dt:.2f} s, ' + ', '.join(f'{COCO[c]} {conf:.2f}' for c, conf, _ in found), flush=True)
        n += 1


if __name__ == '__main__':
    main()
