#!/usr/bin/env python3
"""Serve only the embed harness and built game, with correct JS/WASM MIME types.

Build: cd client && trunk build --public-url ./
Run:   python3 tools/mobile-preview/serve.py
Tunnel: ngrok http http://127.0.0.1:8090
"""
import gzip
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[2]
DIST = ROOT / 'client' / 'dist'
HARNESS = Path(__file__).with_name('index.html')

class Handler(SimpleHTTPRequestHandler):
    extensions_map = {**SimpleHTTPRequestHandler.extensions_map,
                      '.wasm': 'application/wasm', '.js': 'text/javascript'}

    def translate_path(self, path):
        route = unquote(urlsplit(path).path)
        if route == '/':
            return str(HARNESS)
        if route.startswith('/game/'):
            candidate = (DIST / route.removeprefix('/game/')).resolve()
            if candidate.is_relative_to(DIST.resolve()):
                return str(candidate)
        return str(DIST / '__not_found__')

    def list_directory(self, path):
        self.send_error(404)
        return None

    def send_head(self):
        path = Path(self.translate_path(self.path))
        if path.is_dir():
            path = path / 'index.html'
        if not path.is_file():
            self.send_error(404)
            return None
        import io
        content = path.read_bytes()
        compressed = 'gzip' in self.headers.get('Accept-Encoding', '') and path.suffix in ('.wasm', '.js', '.html')
        if compressed:
            content = gzip.compress(content, compresslevel=1)
        self.send_response(200)
        self.send_header('Content-Type', self.guess_type(str(path)))
        self.send_header('Content-Length', str(len(content)))
        self.send_header('Cache-Control', 'no-cache')
        self.send_header('Vary', 'Accept-Encoding')
        if compressed:
            self.send_header('Content-Encoding', 'gzip')
        self.end_headers()
        return io.BytesIO(content)

if __name__ == '__main__':
    print('Mobile preview: http://127.0.0.1:8090', flush=True)
    ThreadingHTTPServer(('127.0.0.1', 8090), Handler).serve_forever()
