#!/bin/bash
# xtafkit-agent: Host command bridge for xtafkit development
#
# Bridges the gap between a sandboxed development environment and the macOS
# host. Watches .agent/request.json for commands, executes them on the host,
# and writes results to .agent/response.json.
#
# Supported commands:
#   shell     — Run any shell command on the host (git, cargo fmt, system tools)
#   build     — cargo build --release
#   cargo-test — cargo test --workspace
#   ftp-ls    — List a directory on an Xbox 360 via FTP
#   ftp-get   — Download a file from Xbox 360 via FTP
#   ftp-scan  — Scan local network for FTP servers
#   <other>   — Passed to the xtafkit CLI as: xtafkit --json <command> <device> [args...]
#
# Usage: sudo bash .agent/agent.sh [device]
#   device: e.g. /dev/rdisk4 (default)

# NOTE: no set -e — we want the agent to survive xtafkit errors

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CLI="$PROJECT_DIR/target/release/xtafkit"
AGENT_DIR="$SCRIPT_DIR"
REQUEST="$AGENT_DIR/request.json"
RESPONSE="$AGENT_DIR/response.json"
LOCK="$AGENT_DIR/processing"
DEVICE="${1:-/dev/rdisk4}"

if [ ! -f "$CLI" ]; then
    echo "WARN: xtafkit not found at $CLI — will build on first request or 'build' command."
fi

echo "╔══════════════════════════════════════════════╗"
echo "║  xtafkit-agent — Host Command Bridge         ║"
echo "╠══════════════════════════════════════════════╣"
echo "║  Project: $PROJECT_DIR"
echo "║  Device:  $DEVICE"
echo "║  CLI:     $CLI"
echo "║  Watching: $REQUEST"
echo "╠══════════════════════════════════════════════╣"
echo "║  Commands: shell, build, cargo-test,         ║"
echo "║   ftp-ls, ftp-get, ftp-scan, or xtafkit <cmd>║"
echo "╚══════════════════════════════════════════════╝"
echo ""
echo "Waiting for commands... (Ctrl+C to stop)"
echo ""

# Clean up any stale files
rm -f "$REQUEST" "$RESPONSE" "$LOCK"

# Write a ready marker
echo '{"status":"ready"}' > "$RESPONSE"

while true; do
    # Wait for request file to appear
    if [ -f "$REQUEST" ] && [ ! -f "$LOCK" ]; then
        # Mark as processing
        touch "$LOCK"

        # Read the request
        CMD=$(cat "$REQUEST" 2>/dev/null || true)
        rm -f "$REQUEST"

        if [ -z "$CMD" ]; then
            echo '{"error":"empty request"}' > "$RESPONSE"
            rm -f "$LOCK"
            continue
        fi

        # Parse JSON using python3 — outputs a NUL-separated arg list for safe handling
        PARSED=$(echo "$CMD" | python3 -c "
import sys, json
d = json.load(sys.stdin)
cmd = d.get('command', '')
args = d.get('args', [])
print(cmd)
print(len(args))
for a in args:
    print(a)
" 2>/dev/null || true)

        COMMAND=$(echo "$PARSED" | head -1)
        ARG_COUNT=$(echo "$PARSED" | sed -n '2p')

        if [ -z "$COMMAND" ]; then
            echo '{"error":"no command field in request"}' > "$RESPONSE"
            rm -f "$LOCK"
            continue
        fi

        # Read args into array
        EXTRA_ARGS=()
        if [ -n "$ARG_COUNT" ] && [ "$ARG_COUNT" -gt 0 ] 2>/dev/null; then
            IDX=0
            while IFS= read -r line; do
                EXTRA_ARGS+=("$line")
                IDX=$((IDX + 1))
                [ "$IDX" -ge "$ARG_COUNT" ] && break
            done <<< "$(echo "$PARSED" | tail -n +3)"
        fi

        TIMESTAMP=$(date '+%H:%M:%S')

        # Handle special meta-commands
        if [ "$COMMAND" = "build" ]; then
            echo "[$TIMESTAMP] > cargo build --release"
            OUTPUT=$(cd "$PROJECT_DIR" && cargo build --release 2>&1) || true
            EXIT_CODE=$?
            if [ $EXIT_CODE -eq 0 ]; then
                OUTPUT="{\"status\":\"ok\",\"message\":\"Build succeeded\"}"
            else
                # Escape the build output for JSON
                ESCAPED=$(echo "$OUTPUT" | python3 -c "import sys,json; print(json.dumps(sys.stdin.read()))" 2>/dev/null || echo "\"build failed\"")
                OUTPUT="{\"status\":\"error\",\"message\":$ESCAPED}"
            fi
            echo "[$TIMESTAMP]   Build done (exit=$EXIT_CODE)"
        elif [ "$COMMAND" = "ftp-ls" ]; then
            # List a directory on the Xbox 360 via FTP
            # Args: [host] [path] [user] [pass] [port]
            FTP_HOST="${EXTRA_ARGS[0]:-}"
            FTP_PATH="${EXTRA_ARGS[1]:-/}"
            FTP_USER="${EXTRA_ARGS[2]:-xboxftp}"
            FTP_PASS="${EXTRA_ARGS[3]:-123456}"
            FTP_PORT="${EXTRA_ARGS[4]:-21}"

            if [ -z "$FTP_HOST" ]; then
                OUTPUT='{"error":"ftp-ls requires host as first arg"}'
            else
                echo "[$TIMESTAMP] > ftp-ls ${FTP_HOST}:${FTP_PORT} ${FTP_PATH}"
                RAW=$(curl -s --max-time 15 --user "${FTP_USER}:${FTP_PASS}" \
                    "ftp://${FTP_HOST}:${FTP_PORT}${FTP_PATH}/" 2>&1) || true
                ESCAPED=$(echo "$RAW" | python3 -c "import sys,json; print(json.dumps(sys.stdin.read()))" 2>/dev/null || echo "\"ftp error\"")
                OUTPUT="{\"status\":\"ok\",\"path\":$(python3 -c "import json; print(json.dumps('$FTP_PATH'))" 2>/dev/null),\"listing\":$ESCAPED}"
                echo "[$TIMESTAMP]   FTP ls done"
            fi
        elif [ "$COMMAND" = "ftp-scan" ]; then
            # Scan local network for FTP servers (Xbox 360 with Aurora/FSD)
            echo "[$TIMESTAMP] > ftp-scan"
            LOCAL_IP=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || echo "")
            if [ -z "$LOCAL_IP" ]; then
                OUTPUT='{"error":"could not determine local IP"}'
            else
                SUBNET=$(echo "$LOCAL_IP" | sed 's/\.[0-9]*$/./')
                SCAN_TMP=$(mktemp)
                for i in $(seq 1 254); do
                    ip="${SUBNET}${i}"
                    [ "$ip" = "$LOCAL_IP" ] && continue
                    ( nc -z -w 1 "$ip" 21 2>/dev/null && echo "$ip" >> "$SCAN_TMP" ) &
                done
                wait 2>/dev/null
                HOSTS="[]"
                if [ -s "$SCAN_TMP" ]; then
                    HOSTS=$(cat "$SCAN_TMP" | python3 -c "import sys,json; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))" 2>/dev/null || echo "[]")
                fi
                rm -f "$SCAN_TMP"
                OUTPUT="{\"status\":\"ok\",\"local_ip\":\"$LOCAL_IP\",\"ftp_hosts\":$HOSTS}"
                echo "[$TIMESTAMP]   FTP scan done"
            fi
        elif [ "$COMMAND" = "ftp-get" ]; then
            # Download a file from Xbox 360 via FTP and return base64-encoded content
            # Args: [host] [path] [user] [pass] [port]
            FTP_HOST="${EXTRA_ARGS[0]:-}"
            FTP_PATH="${EXTRA_ARGS[1]:-}"
            FTP_USER="${EXTRA_ARGS[2]:-xboxftp}"
            FTP_PASS="${EXTRA_ARGS[3]:-123456}"
            FTP_PORT="${EXTRA_ARGS[4]:-21}"

            if [ -z "$FTP_HOST" ] || [ -z "$FTP_PATH" ]; then
                OUTPUT='{"error":"ftp-get requires host and path args"}'
            else
                echo "[$TIMESTAMP] > ftp-get ${FTP_HOST}:${FTP_PORT} ${FTP_PATH}"
                TMP_FILE=$(mktemp)
                curl -s --max-time 30 --user "${FTP_USER}:${FTP_PASS}" \
                    "ftp://${FTP_HOST}:${FTP_PORT}${FTP_PATH}" -o "$TMP_FILE" 2>&1 || true
                if [ -s "$TMP_FILE" ]; then
                    B64=$(base64 < "$TMP_FILE")
                    SIZE=$(stat -f%z "$TMP_FILE" 2>/dev/null || wc -c < "$TMP_FILE")
                    ESCAPED=$(echo "$B64" | python3 -c "import sys,json; print(json.dumps(sys.stdin.read().strip()))" 2>/dev/null)
                    OUTPUT="{\"status\":\"ok\",\"size\":$SIZE,\"base64\":$ESCAPED}"
                else
                    OUTPUT='{"error":"ftp download failed or file empty"}'
                fi
                rm -f "$TMP_FILE"
                echo "[$TIMESTAMP]   FTP get done"
            fi
        elif [ "$COMMAND" = "shell" ]; then
            # Run an arbitrary shell command on the Mac (use carefully)
            SHELL_CMD="${EXTRA_ARGS[0]:-}"
            if [ -z "$SHELL_CMD" ]; then
                OUTPUT='{"error":"shell requires a command as first arg"}'
            else
                echo "[$TIMESTAMP] > shell: $SHELL_CMD"
                RAW=$(bash -c "$SHELL_CMD" 2>&1) || true
                ESCAPED=$(echo "$RAW" | python3 -c "import sys,json; print(json.dumps(sys.stdin.read()))" 2>/dev/null || echo "\"command failed\"")
                OUTPUT="{\"status\":\"ok\",\"output\":$ESCAPED}"
                echo "[$TIMESTAMP]   Shell done"
            fi
        elif [ "$COMMAND" = "cargo-test" ]; then
            echo "[$TIMESTAMP] > cargo test --workspace"
            OUTPUT=$(cd "$PROJECT_DIR" && cargo test --workspace 2>&1) || true
            EXIT_CODE=$?
            if [ $EXIT_CODE -eq 0 ]; then
                ESCAPED=$(echo "$OUTPUT" | python3 -c "import sys,json; print(json.dumps(sys.stdin.read()))" 2>/dev/null || echo "\"test output\"")
                OUTPUT="{\"status\":\"ok\",\"message\":$ESCAPED}"
            else
                ESCAPED=$(echo "$OUTPUT" | python3 -c "import sys,json; print(json.dumps(sys.stdin.read()))" 2>/dev/null || echo "\"test failed\"")
                OUTPUT="{\"status\":\"error\",\"message\":$ESCAPED}"
            fi
            echo "[$TIMESTAMP]   Test done (exit=$EXIT_CODE)"
        else
            echo "[$TIMESTAMP] > xtafkit --json $COMMAND $DEVICE ${EXTRA_ARGS[*]}"

            # Execute — capture both stdout and stderr, never let it kill the agent
            OUTPUT=$("$CLI" --json "$COMMAND" "$DEVICE" "${EXTRA_ARGS[@]}" 2>&1) || true
            EXIT_CODE=${PIPESTATUS[0]:-$?}

            if [ -z "$OUTPUT" ]; then
                OUTPUT="{\"error\":\"xtafkit exited with code $EXIT_CODE and no output\"}"
            fi
            echo "[$TIMESTAMP]   Done (exit=$EXIT_CODE, ${#OUTPUT} bytes)"
        fi

        # Write response
        echo "$OUTPUT" > "$RESPONSE"

        rm -f "$LOCK"
    fi

    # Poll interval
    sleep 0.3
done
