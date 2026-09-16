"""Local stdio MCP fixture: one discoverable tool, no files or network."""
import json
import sys


for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": request.get("params", {}).get("protocolVersion", "2024-11-05"),
                  "capabilities": {"tools": {}}, "serverInfo": {"name": "emp-fixture", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "calendar_lookup", "description": "Lookup the fixture calendar and return its test marker",
                             "annotations": {"readOnlyHint": True, "destructiveHint": False, "openWorldHint": False},
                             "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "EMP_RUNTIME_TOOL_OK"}], "isError": False}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
