#!/usr/bin/env python3
"""Serve a local browser console for the h2c storage example."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

import httpx


HTML = Path(__file__).with_name("h2c_web_console.html")
FORWARDED_HEADERS = ("content-type", "content-length", "allow")


class ConsoleServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, storage_url):
        super().__init__(address, ConsoleHandler)
        self.storage_url = storage_url.rstrip("/")
        self.client = httpx.Client(http1=False, http2=True, trust_env=False)

    def server_close(self):
        self.client.close()
        super().server_close()


class ConsoleHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/":
            payload = HTML.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(payload)
            return
        self.proxy("GET")

    def do_HEAD(self):
        self.proxy("HEAD")

    def do_PUT(self):
        self.proxy("PUT")

    def do_DELETE(self):
        self.proxy("DELETE")

    def do_POST(self):
        self.proxy("POST")

    def proxy(self, method):
        route = urlsplit(self.path)
        if route.path != "/api/objects" and not route.path.startswith("/api/objects/"):
            self.send_error(404, "Unknown route")
            return
        if route.path.startswith("/api/objects/") and method == "PUT":
            raw_length = self.headers.get("Content-Length")
            if raw_length is None or not raw_length.isdecimal():
                self.send_error(411, "PUT requires Content-Length")
                return
            body = self.rfile.read(int(raw_length))
        else:
            body = None

        path = route.path.removeprefix("/api")
        target = self.server.storage_url + path
        if route.query:
            target += "?" + route.query
        headers = {}
        if method == "PUT" and "Content-Type" in self.headers:
            headers["Content-Type"] = self.headers["Content-Type"]

        try:
            with self.server.client.stream(method, target, headers=headers, content=body) as response:
                self.send_response(response.status_code)
                for name in FORWARDED_HEADERS:
                    if name in response.headers:
                        self.send_header(name, response.headers[name])
                self.send_header("X-Upstream-HTTP-Version", response.http_version)
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                if method != "HEAD":
                    for chunk in response.iter_raw():
                        self.wfile.write(chunk)
        except httpx.TimeoutException as error:
            self.send_upstream_error(504, error)
        except httpx.HTTPError as error:
            self.send_upstream_error(502, error)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def send_upstream_error(self, status, error):
        self.log_error("Upstream request failed: %s", error)
        if self.headers_sent:
            return
        payload = f"Storage server request failed: {error}\n".encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(payload)

    def send_response(self, code, message=None):
        self.headers_sent = True
        super().send_response(code, message)

    def handle_one_request(self):
        self.headers_sent = False
        super().handle_one_request()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", default="127.0.0.1:8082", metavar="HOST:PORT")
    parser.add_argument("--storage-url", default="http://127.0.0.1:8081")
    args = parser.parse_args()
    host, separator, port = args.listen.rpartition(":")
    if not separator or not host or not port.isdecimal():
        parser.error("--listen must be HOST:PORT")
    storage = urlsplit(args.storage_url)
    if storage.scheme != "http" or not storage.hostname or storage.path not in ("", "/") or storage.query or storage.fragment:
        parser.error("--storage-url must be an http://HOST:PORT origin")
    if storage.username or storage.password:
        parser.error("--storage-url must not contain credentials")
    try:
        storage_port = storage.port
    except ValueError:
        parser.error("--storage-url must have a valid port")
    if storage_port is None:
        parser.error("--storage-url must include a port")

    with ConsoleServer((host, int(port)), args.storage_url) as server:
        bound_host, bound_port = server.server_address[:2]
        print(f"Browser console: http://{bound_host}:{bound_port}/", flush=True)
        print(f"Storage server: {server.storage_url}", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
