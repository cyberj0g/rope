#!/bin/sh

while IFS= read -r message; do
    id=$(printf '%s\n' "$message" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
    case "$message" in
        *'"method":"server/discover"'*)
            printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"not found"}}\n' "$id"
            ;;
        *'"method":"initialize"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id"
            ;;
        *'"method":"tools/list"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo a value","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]}}\n' "$id"
            ;;
        *'"method":"tools/call"'*)
            value=$(printf '%s\n' "$message" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: %s"}],"isError":false}}\n' "$id" "$value"
            ;;
    esac
done
