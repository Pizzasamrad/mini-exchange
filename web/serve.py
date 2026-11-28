#!/usr/bin/env python3
"""Serve the demo with a wasm MIME type.

`python -m http.server` on some builds labels `.wasm` as octet-stream, and the
browser then refuses to compile the module.
"""

import argparse
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class Handler(SimpleHTTPRequestHandler):
    extensions_map = {
        **SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Serve the mini-exchange demo")
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8080)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    server = ThreadingHTTPServer(
        (args.bind, args.port), partial(Handler, directory=str(root))
    )
    print(f"http://{args.bind}:{args.port}/")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
