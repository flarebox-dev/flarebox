#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import argparse
import functools
import http.server
from pathlib import Path
import socketserver
import ssl
import subprocess


class Server(socketserver.ThreadingMixIn, socketserver.TCPServer):
    daemon_threads = True


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        with self.server.requests.open("a") as requests:
            requests.write(self.path + "\n")
        if self.server.redirect and (not self.server.only or self.path.rsplit("/", 1)[-1] in self.server.only):
            self.send_response(302)
            self.send_header("Location", self.server.redirect + self.path)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        super().do_GET()

    def log_message(self, *args):
        pass


def certificate(directory):
    cert, key = directory / "cert.pem", directory / "key.pem"
    if not cert.exists():
        directory.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=127.0.0.1",
             "-addext", "subjectAltName=IP:127.0.0.1", "-keyout", str(key), "-out", str(cert)],
            check=True, capture_output=True,
        )
    return cert, key


def main():
    parser = argparse.ArgumentParser(description="Serve a release directory on 127.0.0.1 and log each request path.")
    parser.add_argument("root", help="directory to serve")
    parser.add_argument("state", help="directory that receives origin (written once listening) and requests")
    parser.add_argument("--tls", metavar="DIR", help="serve HTTPS with DIR/cert.pem, created when missing")
    parser.add_argument("--redirect", metavar="ORIGIN", help="answer with a 302 to ORIGIN followed by the request path")
    parser.add_argument("--only", action="append", default=[], metavar="NAME", help="redirect only paths ending in NAME")
    args = parser.parse_args()
    server = Server(("127.0.0.1", 0), functools.partial(Handler, directory=args.root))
    scheme = "http"
    if args.tls:
        cert, key = certificate(Path(args.tls))
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(str(cert), str(key))
        server.socket = context.wrap_socket(server.socket, server_side=True)
        scheme = "https"
    state = Path(args.state)
    state.mkdir(parents=True, exist_ok=True)
    server.requests = state / "requests"
    server.requests.touch()
    server.redirect, server.only = args.redirect, set(args.only)
    (state / "origin.tmp").write_text(f"{scheme}://127.0.0.1:{server.server_address[1]}")
    (state / "origin.tmp").rename(state / "origin")
    server.serve_forever()


if __name__ == "__main__":
    main()
