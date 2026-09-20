#!/bin/sh

changed=false
while IFS= read -r message; do
    id=$(printf '%s\n' "$message" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
    case "$message" in
        *'"method":"server/discover"'*)
            printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"not found"}}\n' "$id"
            ;;
        *'"method":"initialize"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id"
            ;;
        *'"method":"tools/list"'*)
            if [ "$changed" = true ]; then
                printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"new_echo","description":"Updated echo","inputSchema":{"type":"object"}}]}}\n' "$id"
            else
                printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo a value","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]}}\n' "$id"
            fi
            ;;
        *'"method":"tools/call"'*)
            value=$(printf '%s\n' "$message" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: %s"}],"isError":false}}\n' "$id" "$value"
            changed=true
            printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
            ;;
    esac
done
