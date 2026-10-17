# /// script
# requires-python = ">=3.10"
# dependencies = ["mcp>=2,<3"]
# ///
"""MCP server that reads live scope timings from an app running a ProfileServer.

The app binds `profiler::ProfileServer` and polls it; each tool call here sends it one
request line and returns the JSON reply. PROFILER_ADDR (host:port) overrides the address.
"""

import os
import socket

from mcp.server.mcpserver import MCPServer

ADDRESS = os.environ.get("PROFILER_ADDR", "127.0.0.1:5600")
# The app answers on its next poll, so allow for a slow frame.
TIMEOUT_SECONDS = 5.0

mcp = MCPServer("profiler")


def ask(request: str) -> str:
    host, port = ADDRESS.rsplit(":", 1)
    try:
        with socket.create_connection((host, int(port)), TIMEOUT_SECONDS) as connection:
            connection.sendall(f"{request}\n".encode())
            with connection.makefile("r", encoding="utf-8") as reply:
                line = reply.readline()
    except OSError as error:
        raise RuntimeError(
            f"no profiled app answering on {ADDRESS} ({error}): is it running "
            "and calling ProfileServer::poll?"
        ) from error
    if not line:
        raise RuntimeError(f"the app on {ADDRESS} closed the connection without replying")
    return line


@mcp.tool()
def profile_tree(recent: bool = True) -> str:
    """Scope timings as a tree: hits, total_ms (including children) and self_ms.

    With recent, only scopes that ended in the profiler's window (10 s by default);
    otherwise everything since the app started. Hotspots are the largest self_ms.
    """
    return ask("recent" if recent else "snapshot")


@mcp.tool()
def profile_series() -> str:
    """Self time of each scope path in every bucket (bucket_ms) of the window.

    Oldest bucket first. Use it to spot spikes and trends that a total hides.
    """
    return ask("series")


if __name__ == "__main__":
    mcp.run()
