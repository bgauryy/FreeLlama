#!/usr/bin/env python3
"""Tiny MCP stdio fixture server for adapter integration tests."""

from __future__ import annotations

import argparse
import json
import os
import sys

TOOLS = {"atlas.build_code": {"build_code": os.environ.get("FREELLAMA_MCP_BUILD_CODE", "MCP-2048")}, "atlas.checksum": {"checksum": os.environ.get("FREELLAMA_MCP_CHECKSUM", "7f3a9c1d")}}

argparse.ArgumentParser(description="Run the FreeLlama Atlas MCP fixture server over stdio.").parse_args()


def response(request: dict) -> dict:
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": request.get("params", {}).get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "freellama-atlas", "version": "1.0.0"}}
    elif method == "notifications/initialized":
        return {}
    elif method == "tools/list":
        result = {"tools": [{"name": name, "description": f"Return Atlas fixture {name.rsplit('.', 1)[-1]}", "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}} for name in sorted(TOOLS)]}
    elif method == "tools/call":
        name = request.get("params", {}).get("name")
        if name not in TOOLS:
            raise KeyError(f"unknown tool: {name}")
        result = {"content": [{"type": "text", "text": json.dumps(TOOLS[name], sort_keys=True)}], "structuredContent": TOOLS[name], "isError": False}
    else:
        raise KeyError(f"unknown method: {method}")
    return {"jsonrpc": "2.0", "id": request.get("id"), "result": result}

for line in sys.stdin:
    try:
        request = json.loads(line)
        result = response(request)
        if result:
            print(json.dumps(result), flush=True)
    except Exception as error:
        print(json.dumps({"jsonrpc": "2.0", "id": request.get("id") if 'request' in locals() else None, "error": {"code": -32601, "message": str(error)}}), flush=True)
