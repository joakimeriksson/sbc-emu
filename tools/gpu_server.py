#!/usr/bin/env python3
"""Host side of the GPU tests: serves tests/gpu/ to the guest (http://10.0.2.2:PORT/ through the
emulated NAT) and stores what the guest PUTs under out/gpu-results/.
    tools/gpu_server.py [port]      (default 8800)"""
import http.server, os, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
SRC = os.path.join(ROOT, 'tests', 'gpu')
OUT = os.path.join(ROOT, 'out', 'gpu-results')

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=SRC, **k)
    def do_PUT(self):
        name = os.path.basename(self.path)
        os.makedirs(OUT, exist_ok=True)
        n = int(self.headers.get('Content-Length', 0))
        with open(os.path.join(OUT, name), 'wb') as f:
            f.write(self.rfile.read(n))
        self.send_response(201); self.end_headers()
    def log_message(self, *a):
        pass

port = int(sys.argv[1]) if len(sys.argv) > 1 else 8800
http.server.ThreadingHTTPServer(('127.0.0.1', port), Handler).serve_forever()
