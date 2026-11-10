#!/usr/bin/env python3
"""Isolated taskq/log fixture. Build taskq; start dashboard on 18080; run this.
No live configuration or database is used. Ctrl-C cleans up the fixture server.
"""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
import urllib.request

root = Path(__file__).resolve().parents[3]
with tempfile.TemporaryDirectory(prefix="taskq-log-fixture-") as tmp:
    scratch = Path(tmp)
    config = scratch / "config.toml"
    config.write_text('[server]\ncors_origins = ["http://127.0.0.1:18080"]\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith("TASKQ_")}
    env.update(TASKQ_CONFIG=str(config), TASKQ_DB=str(scratch / "taskq.db"), TASKQ_TOKEN_FILE=str(scratch / "no-token"))
    server = subprocess.Popen([str(root / "taskq/target/debug/taskq"), "server", "--listen", "127.0.0.1:0"], env=env, stdout=subprocess.PIPE, text=True)
    try:
        url = server.stdout.readline().strip().split()[-1]
        for agent in ["claude", "codex"]:
            body = json.dumps({"project": "harvis", "title": f"{agent.title()} viewer fixture"}).encode()
            with urllib.request.urlopen(urllib.request.Request(url + "/api/tasks", body, {"Content-Type": "application/json"})) as response:
                json.load(response)
        db = sqlite3.connect(scratch / "taskq.db")
        def record(run_id, agent, finished=False):
            path = scratch / f"{run_id}.log"
            path.touch()
            db.execute("INSERT INTO runs (id,task_id,project,agent,mode,claim,started_at,ended_at,exit_code,log_path) VALUES (?,?,?,?,?,?,?,?,?,?)", (run_id, 2 if agent == "codex" else 1, "harvis", agent, "fresh", f"fixture-{run_id}", "2026-10-28T00:00:00Z", "2026-10-28T00:01:00Z" if finished else None, 0 if finished else None, str(path)))
            db.commit()
            return path
        def append(path, value):
            with path.open("a") as f:
                f.write(json.dumps(value) + "\n")
        def message(agent, text):
            if agent == "claude":
                return {"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}
            return {"type": "item.completed", "item": {"type": "agent_message", "text": text}}
        paths = {agent: record(i + 1, agent) for i, agent in enumerate(["claude", "codex"])}
        for i in [3, 4]:
            path = record(i, "claude", True)
            for n in range(1200):
                append(path, message("claude", f"Historical attempt {i}, message {n}"))
        db.execute("UPDATE runs SET exit_code=7 WHERE id=4")
        db.commit()
        print(f"http://127.0.0.1:18080/?taskq={url}", flush=True)
        tick = 0
        while True:
            for agent, path in paths.items():
                append(path, message(agent, f"Fixture message {tick}: inspecting the worktree"))
                if agent == "claude":
                    append(path, {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": f"tool-{tick}", "name": "Bash", "input": {"command": "printf 'fixture output'"}}]}})
                    append(path, {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": f"tool-{tick}", "content": "fixture output\n" * 30}]}})
                else:
                    append(path, {"type": "item.completed", "item": {"type": "command_execution", "id": f"item-{tick}", "command": "printf 'fixture output'", "aggregated_output": "fixture output\n" * 30, "exit_code": 0}})
            tick += 1
            time.sleep(2)
    except KeyboardInterrupt:
        pass
    finally:
        server.terminate()
        server.wait(timeout=10)
