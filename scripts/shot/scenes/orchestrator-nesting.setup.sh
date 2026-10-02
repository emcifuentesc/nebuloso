# ORCHESTRATOR NESTING: one quick prompt launches an orchestrator in the root checkout, and the stand-in
# agent behind it starts three workers with `nebula spawn --worktree`, one checkout apiece. Each worker
# tells its own story: the first keeps running, the second stops on a permission prompt, the third
# finishes. So the root's band shows the orchestrator with its tally (`1 running · 1 waiting · 1 done`)
# and a red dot, its workers under it on a `└`, and each worker's own band lists it again with
# `↑Orchestrate auth fixes`. The PREWARM POOL is off so every launch is one of the scripted ones.
mkdir -p "$WORK/data"
cat > "$WORK/data/config.json" <<'JSON'
{"prewarm_agents": false, "prewarm_sessions": false, "session_pane": "bottom"}
JSON
export NEBULA_SHOT_BIN="$BIN" NEBULA_SHOT_COUNTER="$RUNTIME/launches"
cat > "$RUNTIME/agent" <<'AGENT'
#!/bin/sh
# Stand-in agent: launch 1 is the orchestrator, launches 2-4 its workers, in the order it spawns them.
n=$(cat "$NEBULA_SHOT_COUNTER" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$NEBULA_SHOT_COUNTER"
post() {
  curl -sS -m 3 -X POST -H "Authorization: Bearer $NEBULA_API_TOKEN" -H 'Content-Type: application/json' \
    -d "$2" "$NEBULA_API_URL/api/hooks/claude?agentId=$NEBULA_AGENT_ID&hookEvent=$1" >/dev/null 2>&1
}
case "$n" in
  1) title="Orchestrate auth fixes"
     post UserPromptSubmit '{"session_id":"shot-1","prompt":"Split the auth fixes across three workers"}'
     ( "$NEBULA_SHOT_BIN" spawn --worktree fix-redirect --base main "Fix the login redirect loop"
       "$NEBULA_SHOT_BIN" spawn --worktree token-refresh --base main "Refresh tokens before they expire"
       "$NEBULA_SHOT_BIN" spawn --worktree session-tests --base main "Cover session expiry with tests"
     ) >/dev/null 2>&1 & ;;
  2) title="Login redirect loop"
     post UserPromptSubmit '{"session_id":"shot-2","prompt":"Fix the login redirect loop"}' ;;
  3) title="Token refresh"
     post UserPromptSubmit '{"session_id":"shot-3","prompt":"Refresh tokens before they expire"}'
     sleep 0.3; post PermissionRequest '{"session_id":"shot-3","tool_name":"Bash"}' ;;
  *) title="Session expiry tests"
     post UserPromptSubmit '{"session_id":"shot-4","prompt":"Cover session expiry with tests"}'
     sleep 0.3; post Stop '{"session_id":"shot-4"}' ;;
esac
"$NEBULA_SHOT_BIN" rename "$title" >/dev/null 2>&1 || true
exec /bin/cat
AGENT
chmod +x "$RUNTIME/agent"
export NEBULA_AGENT_CMD="$RUNTIME/agent"
