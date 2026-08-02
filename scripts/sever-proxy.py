#!/usr/bin/env python3
"""Forward one HTTP request upstream, then sever the connection before the reply.

Stands in for a network that dies after the server committed a save: the client
never learns the outcome, so the drill can prove the retry is idempotent.
"""

import socket
import sys
import threading


def pump(src, dst):
    try:
        while chunk := src.recv(65536):
            dst.sendall(chunk)
    except OSError:
        pass


def handle(client, upstream_addr):
    try:
        upstream = socket.create_connection(upstream_addr, timeout=30)
    except OSError:
        client.close()
        return
    threading.Thread(target=pump, args=(client, upstream), daemon=True).start()
    try:
        upstream.recv(65536)
    except OSError:
        pass
    client.close()
    upstream.close()


def main():
    listen_port, upstream_host, upstream_port = (
        int(sys.argv[1]),
        sys.argv[2],
        int(sys.argv[3]),
    )
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", listen_port))
    server.listen(8)
    print("ready", flush=True)
    while True:
        client, _ = server.accept()
        threading.Thread(
            target=handle, args=(client, (upstream_host, upstream_port)), daemon=True
        ).start()


if __name__ == "__main__":
    main()
