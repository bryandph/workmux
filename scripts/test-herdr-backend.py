#!/usr/bin/env python3
"""Opt-in smoke test against an explicitly supplied, disposable Herdr server.

Usage: python scripts/test-herdr-backend.py --socket /tmp/test/herdr.sock
Uses synthetic agent reports; never launches a model or touches existing checkouts.
"""

import argparse
import json
import os
from pathlib import Path
import socket
import shlex
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--binary", default="target/debug/workmux")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    root = Path(tempfile.mkdtemp(prefix="workmux-herdr-smoke-", dir="/tmp")).resolve()
    repo = root / "repo"
    repo.mkdir()
    task = "backend-" + root.name[-8:].replace("_", "x")
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(("HERDR_", "WORKMUX_STATUS_")) or key in (
            "TMUX",
            "TMUX_PANE",
        ):
            env.pop(key, None)
    env.update(
        WORKMUX_BACKEND="herdr",
        HERDR_SOCKET_PATH=args.socket,
        XDG_STATE_HOME=str(root / "state"),
        XDG_CONFIG_HOME=str(root / "config"),
        GIT_CONFIG_GLOBAL="/dev/null",
        GIT_CONFIG_NOSYSTEM="1",
    )
    config = root / "config" / "workmux"
    config.mkdir(parents=True)
    (config / "config.yaml").write_text("mode: window\npanes: []\n")

    def run(*cmd, cwd=repo, extra_env=None, check=True):
        result = subprocess.run(
            cmd,
            cwd=cwd,
            env=env | (extra_env or {}),
            text=True,
            capture_output=True,
            timeout=30,
        )
        if check and result.returncode:
            if cmd[0] == binary:
                for pane in api("pane.list")["panes"]:
                    if pane["cwd"].startswith(str(root)):
                        print(
                            api(
                                "pane.read",
                                pane_id=pane["pane_id"],
                                source="recent_unwrapped",
                                lines=40,
                            )["read"]["text"],
                            flush=True,
                        )
            raise AssertionError(f"{cmd}: {result.stdout}\n{result.stderr}")
        return result

    def api(method, **params):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(10)
            conn.connect(args.socket)
            conn.sendall(
                (
                    json.dumps({"id": "smoke", "method": method, "params": params})
                    + "\n"
                ).encode()
            )
            response = json.loads(conn.makefile().readline())
            if "error" in response:
                raise AssertionError(f"{method}: {response['error']}")
            return response["result"]

    def workspace():
        return next(
            w for w in api("workspace.list")["workspaces"] if w["label"] == "wm-" + task
        )

    for pane in api("pane.list")["panes"]:
        if not pane["cwd"].startswith(
            ("/tmp/workmux-herdr", "/private/tmp/workmux-herdr")
        ):
            raise SystemExit(
                "Refusing test server with non-test panes. Isolate XDG_CONFIG_HOME as well as the socket."
            )

    run("git", "init", "-b", "main")
    run("git", "config", "user.name", "Backend Test")
    run("git", "config", "user.email", "backend-test@example.invalid")
    run("git", "config", "commit.gpgsign", "false")
    run("git", "config", "core.hooksPath", "/dev/null")
    (repo / ".workmux.yaml").write_text("mode: window\nmain_branch: main\npanes: []\n")
    run("git", "add", ".workmux.yaml")
    run("git", "commit", "-m", "test fixture")
    print(f"Smoke fixture: {root}", flush=True)

    run(
        binary,
        "add",
        task,
        "--background",
        "--no-pane-cmds",
        "--no-hooks",
        "--no-file-ops",
    )
    ws = workspace()
    wt = Path(ws["worktree"]["checkout_path"])
    assert wt.is_dir() and wt != repo
    assert ws.get("worktree"), "existing checkout must have Herdr provenance"
    assert ws.get("tokens", {}).get("workmux_owner"), "workspace must carry ownership"
    pane = next(
        p for p in api("pane.list")["panes"] if p["workspace_id"] == ws["workspace_id"]
    )
    pane_id = pane["pane_id"]
    pane_env = {"HERDR_PANE_ID": pane_id, "HERDR_WORKSPACE_ID": ws["workspace_id"]}
    print("PASS add: Git checkout, Herdr provenance, ownership token", flush=True)

    # A real foreground process with synthetic lifecycle reports, without a model.
    fake_agent = root / "codex"
    fake_source = root / "fake_agent.c"
    fake_source.write_text(
        '#include <stdio.h>\nint main(void) { char buf[4096]; puts("SMOKE_AGENT_READY"); fflush(stdout); while(fgets(buf, sizeof(buf), stdin)) { puts("HERDR_BACKEND_SMOKE_OK"); fflush(stdout); } return 0; }\n'
    )
    run("cc", str(fake_source), "-o", str(fake_agent))
    api(
        "pane.send_input",
        pane_id=pane_id,
        text=shlex.quote(str(fake_agent)),
        keys=["enter"],
    )
    for _ in range(50):
        if (
            "SMOKE_AGENT_READY"
            in api("pane.read", pane_id=pane_id, source="recent_unwrapped", lines=50)[
                "read"
            ]["text"]
        ):
            break
        time.sleep(0.1)
    else:
        raise AssertionError("synthetic agent did not start")
    api(
        "pane.report_agent",
        pane_id=pane_id,
        source="custom:smoke",
        agent="codex",
        state="idle",
    )
    run(binary, "set-window-status", "done", cwd=wt, extra_env=pane_env)
    run(binary, "send", task, "test prompt")
    stale = run(binary, "wait", task, "--timeout", "1", check=False)
    assert stale.returncode == 1 and "Timeout" in stale.stderr, stale.stderr
    output = ""
    for _ in range(40):
        output = run(binary, "capture", task).stdout
        if "HERDR_BACKEND_SMOKE_OK" in output:
            break
        time.sleep(0.1)
    assert "HERDR_BACKEND_SMOKE_OK" in output, output
    run(binary, "set-window-status", "done", cwd=wt, extra_env=pane_env)
    run(binary, "wait", task, "--timeout", "5")
    print("PASS send/capture/wait through a synthetic agent", flush=True)

    api(
        "pane.report_agent",
        pane_id=pane_id,
        source="custom:smoke",
        agent="codex",
        state="blocked",
    )
    blocked = run(binary, "send", task, "must-not-be-injected", check=False)
    assert blocked.returncode != 0 and "agent_blocked" in blocked.stderr, blocked.stderr
    run(binary, "wait", task, "--timeout", "1")
    print("PASS blocked prompt rejected and previous status restored", flush=True)

    api("workspace.close", workspace_id=ws["workspace_id"])
    assert wt.exists(), "closing UI must not delete checkout"
    run(binary, "open", task)
    reopened = workspace()
    assert reopened["workspace_id"] != ws["workspace_id"]
    run(binary, "remove", task, "--force")
    assert not wt.exists(), "workmux removal must delete checkout"
    assert not any(
        w["workspace_id"] == reopened["workspace_id"]
        for w in api("workspace.list")["workspaces"]
    )
    print("PASS close/reopen/remove preserve lifecycle ownership", flush=True)

    # Exercise normal shell startup/handshakes and preserve workmux's veto hook.
    task += "-hooks"
    (repo / ".workmux.yaml").write_text(
        "mode: window\nmain_branch: main\n"
        "panes:\n  - command: printf 'ROOT_PANE_READY\\n'\n"
        "  - command: printf 'HELPER_PANE_READY\\n'\n    split: horizontal\n    percentage: 40\n    name: helper\n"
        "post_create:\n  - touch created.marker\npre_remove:\n  - test -f allow.remove\n"
    )
    run("git", "add", ".workmux.yaml")
    run("git", "commit", "-m", "pane and hook fixture")
    run(binary, "add", task, "--background")
    ws = workspace()
    wt = Path(ws["worktree"]["checkout_path"])
    assert (wt / "created.marker").exists(), "post-create hook was skipped"
    panes = [
        p for p in api("pane.list")["panes"] if p["workspace_id"] == ws["workspace_id"]
    ]
    assert len(panes) == 2, panes
    for _ in range(50):
        outputs = "\n".join(
            api(
                "pane.read", pane_id=p["pane_id"], source="recent_unwrapped", lines=100
            )["read"]["text"]
            for p in panes
        )
        if "ROOT_PANE_READY" in outputs and "HELPER_PANE_READY" in outputs:
            break
        time.sleep(0.1)
    else:
        raise AssertionError("configured pane commands did not execute")
    caller = {
        "HERDR_PANE_ID": panes[0]["pane_id"],
        "HERDR_WORKSPACE_ID": ws["workspace_id"],
        "HERDR_CONFIG_PATH": str(Path(args.socket).parent / "config.toml"),
    }
    veto = run(binary, "remove", task, "--force", cwd=wt, extra_env=caller, check=False)
    assert veto.returncode != 0 and wt.exists(), (
        "pre-remove veto did not preserve checkout"
    )
    assert workspace()["workspace_id"] == ws["workspace_id"], (
        "veto closed the workspace"
    )
    (wt / "allow.remove").touch()
    run(binary, "remove", task, "--force", cwd=wt, extra_env=caller)
    for _ in range(100):
        if not wt.exists():
            break
        time.sleep(0.1)
    assert not wt.exists()
    print(
        "PASS shell handshakes, split panes, hooks, veto and deferred removal",
        flush=True,
    )


if __name__ == "__main__":
    main()
