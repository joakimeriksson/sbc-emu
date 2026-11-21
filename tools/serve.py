#!/usr/bin/env python3
"""Static file server for web/ with HTTP Range support (the browser reads SD images lazily).
   tools/serve.py [port]   -> http://127.0.0.1:8791/  (/ is boards.html, the front page, as on the site)"""
import http.server, os, re, sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'web')

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=ROOT, **k)

    def end_headers(self):
        self.send_header('Accept-Ranges', 'bytes')
        self.send_header('Cache-Control', 'no-cache')
        super().end_headers()

    def front(self):
        if self.path == '/' or self.path.startswith('/?'):
            self.path = '/boards.html' + self.path[1:]

    def do_GET(self):
        self.front()
        rng = self.headers.get('Range')
        path = self.translate_path(self.path)
        m = rng and re.match(r'bytes=(\d+)-(\d*)$', rng)
        if not m or not os.path.isfile(path):
            return super().do_GET()
        size = os.path.getsize(path)
        start = int(m.group(1))
        end = min(int(m.group(2)) if m.group(2) else size - 1, size - 1)
        if start >= size:
            self.send_response(416); self.send_header('Content-Range', f'bytes */{size}'); self.end_headers(); return
        self.send_response(206)
        self.send_header('Content-Type', self.guess_type(path))
        self.send_header('Content-Range', f'bytes {start}-{end}/{size}')
        self.send_header('Content-Length', str(end - start + 1))
        self.end_headers()
        with open(path, 'rb') as f:
            f.seek(start)
            left = end - start + 1
            while left > 0:
                chunk = f.read(min(left, 1 << 20))
                if not chunk: break
                self.wfile.write(chunk)
                left -= len(chunk)

    def do_HEAD(self):
        self.front()
        return super().do_HEAD()

http.server.ThreadingHTTPServer.allow_reuse_address = True
port = int(sys.argv[1]) if len(sys.argv) > 1 else 8791
print(f'serving {os.path.normpath(ROOT)} on http://127.0.0.1:{port}/ (with Range)')
http.server.ThreadingHTTPServer(('127.0.0.1', port), Handler).serve_forever()
