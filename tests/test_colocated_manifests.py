"""Structural tests for the colocated openab-pty + classic openab shape (#62).

They pin, on the manifests actually shipped in deploy/, the properties the
shape depends on: one image for both sides, secrets with exactly one home,
one shared /workspace and nothing else shared, the same hardening on every
container, no listener on the broker, and the bot kept off the terminal's
tools MCP server.

Based on the tests in openabdev/openab-pty#63 by @Reese-max, retargeted from
the YAML embedded in the doc to the files in deploy/.

Run: python3 -m pytest -q tests/
"""

from __future__ import annotations

import json
import re
import shlex
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
K8S = ROOT / "deploy" / "k8s" / "pod-with-openab.yaml"
K8S_TERMINAL = ROOT / "deploy" / "k8s" / "pod-tailscale.yaml"
ECS = ROOT / "deploy" / "ecs" / "kiro-with-pty.yaml"


def load(path: Path) -> dict:
    return yaml.safe_load(path.read_text(encoding="utf-8"))


# ------------------------------------------------------------------ helpers


def k8s_pod() -> dict:
    return load(K8S)


def k8s_containers(pod: dict) -> dict[str, dict]:
    return {c["name"]: c for c in pod["spec"]["containers"]}


def k8s_env(c: dict) -> dict[str, dict]:
    return {e["name"]: e for e in c.get("env", [])}


def k8s_mounts(c: dict) -> dict[str, str]:
    return {m["mountPath"]: m["name"] for m in c.get("volumeMounts", [])}


def ecs_spec() -> dict:
    return load(ECS)["spec"]


def ecs_containers(spec: dict) -> dict[str, dict]:
    return {c["name"]: c for c in spec["containers"]}


def broker_script(argv: list[str]) -> str:
    """The shell script a broker container runs (last argv element)."""
    assert argv[:3] == ["/usr/bin/tini", "-g", "--"], f"broker must run under tini -g: {argv}"
    assert argv[3:5] == ["/bin/sh", "-c"], f"broker must be a sh -c script: {argv}"
    return argv[-1]


def toml_agent_args(config: str) -> list[str]:
    m = re.search(r"^\s*args\s*=\s*(\[.*\])\s*$", config, re.MULTILINE)
    assert m, "config has no [agent] args"
    return json.loads(m.group(1))


def check_broker_script(script: str, config: str) -> None:
    # Config is rendered from env into the broker's own /tmp and run from there.
    assert '(umask 077; printf \'%s\\n\' "$OPENAB_CONFIG_TOML" > /tmp/openab.toml)' in script
    # A bare umask would be inherited by kiro-cli: every file the bot writes
    # into the shared /workspace would come out 0600 (measured on p1).
    assert not re.search(r"^\s*umask\b", script, re.MULTILINE), "umask must be scoped to the config write"
    assert re.search(r"\bopenab run -c /tmp/openab\.toml\b", script)
    # The token stays a reference in the non-secret config.
    assert 'bot_token = "${DISCORD_BOT_TOKEN}"' in config
    assert "OPENAB_ACP_ENABLED" not in config and "[mcp]" not in config, (
        "the broker must open no listener: the sidecar exposes every loopback port"
    )
    # The bot runs its own kiro agent, and that agent ignores the shared mcp.json.
    args = toml_agent_args(config)
    assert args[:1] == ["acp"] and "--agent" in args
    agent_name = args[args.index("--agent") + 1]
    m = re.search(r"printf '%s\\n' '(\{.*?\})'", script)
    assert m, "broker script must write its kiro agent file"
    agent = json.loads(m.group(1))
    assert agent["name"] == agent_name
    assert agent["includeMcpJson"] is False, (
        "without includeMcpJson=false the bot loads the terminal's `computer` "
        "server and gets a 401 on every session"
    )
    assert f'"$HOME/.kiro/agents/{agent_name}.json"' in script


# --------------------------------------------------------------------- k8s


def test_k8s_is_one_pod_three_containers_hardened():
    pod = k8s_pod()
    assert pod["kind"] == "Pod"
    assert set(k8s_containers(pod)) == {"tailscale", "openab-pty", "openab"}
    spec = pod["spec"]
    assert spec["automountServiceAccountToken"] is False
    assert spec["securityContext"]["runAsNonRoot"] is True
    assert spec["securityContext"]["runAsUser"] == 1000
    for name, c in k8s_containers(pod).items():
        sc = c["securityContext"]
        assert sc["readOnlyRootFilesystem"] is True, name
        assert sc["allowPrivilegeEscalation"] is False, name
        assert sc["capabilities"]["drop"] == ["ALL"], name
        assert "ports" not in c, f"{name} publishes a port"


def test_k8s_bot_crash_restarts_rather_than_ending_the_pod():
    assert k8s_pod()["spec"]["restartPolicy"] in ("OnFailure", "Always")


def test_k8s_keeps_the_terminal_label_for_the_egress_policy():
    assert k8s_pod()["metadata"]["labels"]["app"] == load(K8S_TERMINAL)["metadata"]["labels"]["app"]


def test_k8s_broker_uses_the_terminal_image():
    c = k8s_containers(k8s_pod())
    assert c["openab"]["image"] == c["openab-pty"]["image"]
    assert c["openab"]["image"].startswith("ghcr.io/openabdev/openab-pty:")


def test_k8s_shares_workspace_and_nothing_else():
    pod = k8s_pod()
    c = k8s_containers(pod)
    oab, pty = k8s_mounts(c["openab"]), k8s_mounts(c["openab-pty"])
    assert oab["/workspace"] == pty["/workspace"]
    shared = set(oab.values()) & set(pty.values())
    assert shared == {oab["/workspace"]}, f"volumes shared beyond /workspace: {shared}"
    assert oab["/tmp"] != pty["/tmp"]
    for name in ("openab", "openab-pty"):
        env = k8s_env(c[name])
        # Image default is HOME=/workspace; an override must not move it.
        assert env.get("HOME", {"value": "/workspace"})["value"] == "/workspace"


def test_k8s_each_secret_has_one_home():
    owners = {"TS_AUTHKEY": "tailscale", "PTY_ADMIN_HASH": "openab-pty", "DISCORD_BOT_TOKEN": "openab"}
    for name, c in k8s_containers(k8s_pod()).items():
        for var, e in k8s_env(c).items():
            assert not var.startswith("PTY_FORWARD_"), f"{var} on {name}"
            if "secretKeyRef" in e.get("valueFrom", {}):
                assert owners.get(var) == name, f"secret {var} must not be on {name}"


def test_k8s_broker_socket_is_outside_workspace():
    env = k8s_env(k8s_containers(k8s_pod())["openab"])
    assert env["OPENAB_AGENT_SOCKET"]["value"].startswith("/tmp/")


def test_k8s_broker_script():
    c = k8s_containers(k8s_pod())["openab"]
    script = broker_script(c["command"] + c["args"])
    check_broker_script(script, k8s_env(c)["OPENAB_CONFIG_TOML"]["value"])
    assert script.rstrip().endswith("exec openab run -c /tmp/openab.toml")


# --------------------------------------------------------------------- ECS


def test_ecs_broker_uses_the_terminal_image_everywhere():
    c = ecs_containers(ecs_spec())
    image = c["openab-pty"]["image"]
    assert c["openab"]["image"] == image
    assert c["init-perms"]["image"] == image, "init-perms should not cost a second pull"


def test_ecs_broker_is_non_essential_and_restarts_itself():
    c = ecs_containers(ecs_spec())
    assert c["openab"]["essential"] is False
    assert c["openab-pty"]["essential"] is True
    script = broker_script(c["openab"]["entryPoint"] + c["openab"]["command"])
    assert re.search(r"while :; do\s+openab run -c /tmp/openab\.toml", script), (
        "a non-essential container is never restarted by ECS; the loop is the restart"
    )


def test_ecs_hardening_matches_the_terminal():
    c = ecs_containers(ecs_spec())
    for name in ("openab", "openab-pty"):
        assert c[name]["user"] == "1000"
        assert c[name]["readonlyRootFilesystem"] is True, name
        assert c[name]["linuxParameters"]["capabilitiesDrop"] == ["ALL"], name


def test_ecs_shares_workspace_and_nothing_else():
    c = ecs_containers(ecs_spec())
    oab, pty = c["openab"]["mountPoints"], c["openab-pty"]["mountPoints"]
    assert oab["/workspace"] == pty["/workspace"]
    assert set(oab.values()) & set(pty.values()) == {oab["/workspace"]}
    # init-perms must chown every volume a uid-1000 container writes to.
    chown = c["init-perms"]["command"][0]
    for vol_path in c["init-perms"]["mountPoints"]:
        assert vol_path in shlex.split(chown.split("&&")[0]), f"{vol_path} not chowned"
    assert set(c["init-perms"]["mountPoints"].values()) == set(oab.values()) | set(pty.values())


def test_ecs_each_secret_has_one_home():
    owners = {"TS_AUTHKEY": "tailscale", "PTY_ADMIN_HASH": "openab-pty", "DISCORD_BOT_TOKEN": "openab"}
    for name, c in ecs_containers(ecs_spec()).items():
        for var in c.get("secrets", {}):
            assert owners.get(var) == name, f"secret {var} must not be on {name}"
        for var in c.get("env", {}):
            assert not var.startswith("PTY_FORWARD_"), f"{var} on {name}"


def test_ecs_has_no_task_role():
    assert "taskRoleArn" not in ecs_spec(), (
        "a task role is reachable from every container; the Discord-only shape needs none"
    )


def test_ecs_broker_script():
    c = ecs_containers(ecs_spec())["openab"]
    assert c["env"]["OPENAB_AGENT_SOCKET"].startswith("/tmp/")
    script = broker_script(c["entryPoint"] + c["command"])
    check_broker_script(script, c["env"]["OPENAB_CONFIG_TOML"])


# --------------------------------------------------------------------- doc

DOC = ROOT / "docs" / "colocated-openab.md"


def doc_yaml_pods() -> list[dict]:
    blocks = re.findall(r"```yaml\n(.*?)```", DOC.read_text(encoding="utf-8"), re.DOTALL)
    return [d for b in blocks for d in yaml.safe_load_all(b) if isinstance(d, dict) and d.get("kind") == "Pod"]


def test_doc_links_resolve():
    text = DOC.read_text(encoding="utf-8")
    for target in re.findall(r"\]\((\.\./[^)#]+)\)", text):
        assert (DOC.parent / target).resolve().exists(), f"dead link {target}"


def test_doc_variant_b_follows_the_real_terminal_label():
    pods = doc_yaml_pods()
    assert len(pods) == 1, "the doc carries exactly one Pod sketch (variant B)"
    broker = pods[0]
    terms = broker["spec"]["affinity"]["podAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
    terminal = load(K8S_TERMINAL)
    # A selector that matches nothing leaves the broker unschedulable.
    assert {t["labelSelector"]["matchLabels"]["app"] for t in terms} == {terminal["metadata"]["labels"]["app"]}
    # The broker follows the terminal, never the reverse (mutual required affinity deadlocks).
    assert "affinity" not in terminal["spec"]
    assert broker["spec"]["restartPolicy"] in ("OnFailure", "Always")
    c = broker["spec"]["containers"][0]
    assert c["image"] == k8s_containers(load(K8S_TERMINAL))["openab-pty"]["image"]
    assert c["securityContext"]["readOnlyRootFilesystem"] is True
    ws = [v for v in broker["spec"]["volumes"] if v["name"] == "workspace"][0]
    assert "persistentVolumeClaim" in ws
