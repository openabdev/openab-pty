"""Structural tests for the colocated openab-pty + classic openab shape (#62).

The canonical manifests for this deployment shape live inside
``docs/colocated-openab.md`` as fenced ``yaml`` blocks: the doc is the reviewed
source of truth that clients such as Connect render from, and these tests pin
the properties the issue requires so the document cannot silently drift away
from them:

- one Pod, three containers (tailscale / openab-pty / openab)
- the broker runs from the *same* image as the terminal (entrypoint override)
- platform tokens reach only the ``openab`` container, never as ``PTY_FORWARD_*``
- ``PTY_ADMIN_HASH`` reaches only ``openab-pty``
- both workloads mount the one ``/workspace``
- the hardening of ``pod-tailscale.yaml`` survives on every container
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest
import yaml

DOC = Path(__file__).resolve().parent.parent / "docs" / "colocated-openab.md"
FENCE = re.compile(r"```yaml\n(.*?)```", re.DOTALL)


def _doc_text() -> str:
    return DOC.read_text(encoding="utf-8")


def _yaml_blocks() -> list[str]:
    return FENCE.findall(_doc_text())


def _load_all() -> list[object]:
    docs = []
    for block in _yaml_blocks():
        docs.extend(yaml.safe_load_all(block))
    return [doc for doc in docs if doc]


def _variant_c_pod() -> dict:
    pods = [
        doc
        for doc in _load_all()
        if isinstance(doc, dict)
        and doc.get("kind") == "Pod"
        and {"tailscale", "openab-pty", "openab"} <= set(_container_names(doc))
    ]
    assert len(pods) == 1, "doc must contain exactly one colocated Pod manifest"
    return pods[0]


def _container_names(pod: dict) -> list[str]:
    return [c["name"] for c in pod["spec"]["containers"]]


def _container(pod: dict, name: str) -> dict:
    matches = [c for c in pod["spec"]["containers"] if c["name"] == name]
    assert len(matches) == 1, f"container {name!r} missing from manifest"
    return matches[0]


def _env_names(container: dict) -> list[str]:
    return [e["name"] for e in container.get("env", [])]


def _env_value(container: dict, name: str) -> str | None:
    for entry in container.get("env", []):
        if entry["name"] == name:
            return entry.get("value")
    return None


def _mounted_paths(container: dict) -> dict[str, str]:
    return {m["mountPath"]: m["name"] for m in container.get("volumeMounts", [])}


def _volume_names(pod: dict) -> set[str]:
    return {v["name"] for v in pod["spec"]["volumes"]}


def test_doc_exists_and_has_yaml_manifest():
    assert DOC.is_file(), "docs/colocated-openab.md is missing"
    assert _yaml_blocks(), "doc carries no fenced yaml blocks"


def test_every_yaml_block_parses():
    docs = _load_all()
    assert docs, "no parseable yaml documents found"


def test_variant_c_is_one_pod_three_containers():
    pod = _variant_c_pod()
    names = _container_names(pod)
    for expected in ("tailscale", "openab-pty", "openab"):
        assert expected in names, f"{expected} container missing"
    spec = pod["spec"]
    assert spec.get("automountServiceAccountToken") is False
    assert spec["securityContext"]["runAsUser"] == 1000
    assert spec["securityContext"]["runAsNonRoot"] is True


def test_broker_uses_the_same_image_as_the_terminal():
    pod = _variant_c_pod()
    openab = _container(pod, "openab")
    pty = _container(pod, "openab-pty")
    assert openab["image"] == pty["image"], (
        "the colocated broker must run from the openab-pty image with an "
        "overridden entrypoint — no second ~700 MB pull, identical CLI version"
    )


def test_broker_entrypoint_is_openab_run_with_a_config_ref():
    pod = _variant_c_pod()
    openab = _container(pod, "openab")
    argv = openab.get("command", []) + openab.get("args", [])
    joined = " ".join(str(a) for a in argv)
    assert re.search(r"\bopenab\s+run\b", joined), f"broker argv is not `openab run`: {argv}"
    assert "-c" in argv, "broker must pass its config with -c"
    config_ref = str(argv[argv.index("-c") + 1])
    assert re.match(r"(s3|https)://", config_ref), (
        f"config ref must be remote (s3:// or https://), got {config_ref!r}"
    )


def test_shared_workspace_is_one_volume_at_one_path():
    pod = _variant_c_pod()
    openab_mounts = _mounted_paths(_container(pod, "openab"))
    pty_mounts = _mounted_paths(_container(pod, "openab-pty"))
    assert "/workspace" in openab_mounts and "/workspace" in pty_mounts
    assert openab_mounts["/workspace"] == pty_mounts["/workspace"], (
        "both sides must mount the same volume at /workspace"
    )
    assert openab_mounts["/workspace"] in _volume_names(pod)
    assert _env_value(_container(pod, "openab"), "HOME") == "/workspace"
    assert _env_value(_container(pod, "openab-pty"), "HOME") == "/workspace"


# Every secret env in the colocated pod has exactly one legitimate home.
SECRET_OWNERS = {"TS_AUTHKEY": "tailscale", "PTY_ADMIN_HASH": "openab-pty"}


def test_platform_tokens_reach_only_the_broker_container():
    pod = _variant_c_pod()
    for container in pod["spec"]["containers"]:
        for entry in container.get("env", []):
            name = entry["name"]
            assert not name.startswith("PTY_FORWARD_"), (
                f"{name} is a PTY_FORWARD_* on {container['name']}: platform "
                "tokens must never be forwarded into session environments"
            )
            if "secretKeyRef" not in entry.get("valueFrom", {}):
                continue
            owner = SECRET_OWNERS.get(name)
            if owner is not None:
                assert container["name"] == owner, (
                    f"{name} belongs on {owner}, not {container['name']}"
                )
            else:
                assert container["name"] == "openab", (
                    f"secret {name} is injected into {container['name']}: only "
                    "the broker container may hold platform tokens"
                )
    openab = _container(pod, "openab")
    secret_names = [
        e["name"]
        for e in openab.get("env", [])
        if "secretKeyRef" in e.get("valueFrom", {})
    ]
    assert secret_names, "the broker container must take its tokens from secrets"


def test_admin_hash_reaches_only_the_terminal_container():
    pod = _variant_c_pod()
    for container in pod["spec"]["containers"]:
        names = _env_names(container)
        if container["name"] == "openab-pty":
            assert "PTY_ADMIN_HASH" in names
        else:
            assert "PTY_ADMIN_HASH" not in names, (
                f"PTY_ADMIN_HASH leaked into {container['name']}"
            )


def test_terminal_stays_loopback_only():
    pod = _variant_c_pod()
    pty = _container(pod, "openab-pty")
    # The image default is 127.0.0.1:8090 (Dockerfile ENV), so unset is the
    # same claim as set — what is forbidden is any off-loopback value.
    assert _env_value(pty, "PTY_LISTEN") in (None, "127.0.0.1:8090")
    assert "ports" not in pty, "the terminal must publish nothing — the tailnet sidecar is the only way in"
    tailscale = _container(pod, "tailscale")
    assert _env_value(tailscale, "TS_USERSPACE") == "true"


def test_broker_is_hardened_to_the_same_level_as_the_terminal():
    pod = _variant_c_pod()
    for name in ("openab", "openab-pty", "tailscale"):
        sc = _container(pod, name).get("securityContext", {})
        assert sc.get("readOnlyRootFilesystem") is True, f"{name}: rootfs must be read-only"
        assert sc.get("allowPrivilegeEscalation") is False, f"{name}: no privilege escalation"
        assert "ALL" in sc.get("capabilities", {}).get("drop", []), f"{name}: drop all caps"
    openab_mounts = _mounted_paths(_container(pod, "openab"))
    assert "/tmp" in openab_mounts, "broker needs a writable /tmp under readOnlyRootFilesystem"


def test_doc_states_the_trust_model():
    text = _doc_text()
    for needle in (
        "trust zone",
        "OPENAB_ACP_ENABLED",
        "OPENAB_AGENT_SOCKET",
        "task role",
        "worktree",
    ):
        assert needle.lower() in text.lower(), f"doc must cover {needle!r}"


def test_doc_states_config_path_and_token_rule():
    text = _doc_text()
    assert re.search(r"openab run -c s3://", text)
    assert "PTY_FORWARD_" in text, "doc must name the forbidden token path"


def test_doc_states_the_sizing_formula():
    text = _doc_text()
    for needle in ("1024", "2048", "4096"):
        assert needle in text, f"sizing guidance must mention {needle}"


def _variant_b_broker() -> dict:
    pods = [
        doc
        for doc in _load_all()
        if isinstance(doc, dict)
        and doc.get("kind") == "Pod"
        and "openab" in _container_names(doc)
        and "openab-pty" not in _container_names(doc)
    ]
    assert len(pods) == 1, "doc must contain exactly one variant-B broker pod"
    return pods[0]


def test_variant_b_broker_pins_to_the_real_terminal_label():
    broker = _variant_b_broker()
    terms = broker["spec"]["affinity"]["podAffinity"][
        "requiredDuringSchedulingIgnoredDuringExecution"
    ]
    selected = {t["labelSelector"]["matchLabels"]["app"] for t in terms}
    # The terminal pod is pod-tailscale.yaml unchanged — so the selector must
    # match the label that file actually carries, not a hoped-for one.
    terminal = yaml.safe_load(
        (DOC.parent.parent / "deploy" / "k8s" / "pod-tailscale.yaml").read_text(
            encoding="utf-8"
        )
    )
    terminal_app = terminal["metadata"]["labels"]["app"]
    assert selected == {terminal_app}, (
        f"variant-B podAffinity selects {selected}, but the terminal pod is "
        f"labelled app={terminal_app} — a selector that never matches leaves "
        "the broker unschedulable"
    )
    # The broker follows the terminal pod, never the reverse.
    assert "affinity" not in terminal["spec"], (
        "the terminal pod must stay free of affinity rules — colocating by "
        "mutual required affinity deadlocks both pods"
    )
    # The broker pod keeps the same hardening as every other container.
    sc = _container(broker, "openab").get("securityContext", {})
    assert sc.get("readOnlyRootFilesystem") is True
    assert "ALL" in sc.get("capabilities", {}).get("drop", [])
    ws = broker["spec"]["volumes"]
    pvc = [v for v in ws if v["name"] == "workspace"]
    assert pvc and "persistentVolumeClaim" in pvc[0], (
        "variant B shares the workspace through a RWO PVC, not an emptyDir"
    )
