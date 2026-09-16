"""把远端仓库历史重写为**单个干净提交**。

为什么需要：误发布的内容虽然在新提交里删掉了，但旧提交仍可通过 SHA 访问
（仓库历史里也还列着）。要真正下架，必须让分支指向一个"没有父提交"的全新提交，
使旧提交不再出现在历史里。

用法： python tools/gh-rewrite.py <owner> <repo>
"""
import base64
import json
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

OWNER = sys.argv[1] if len(sys.argv) > 1 else "Li-Mingshuang"
REPO = sys.argv[2] if len(sys.argv) > 2 else "livephoto-browser"
ROOT = Path(__file__).resolve().parent.parent
API = f"https://api.github.com/repos/{OWNER}/{REPO}"
token = subprocess.check_output(["gh", "auth", "token"], text=True).strip()


def req(method, url, data=None):
    body = json.dumps(data).encode() if data is not None else None
    r = urllib.request.Request(
        url,
        data=body,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "User-Agent": "livephoto-rewrite",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(r, timeout=90) as resp:
        raw = resp.read().decode()
        return json.loads(raw) if raw else {}


info = req("GET", API)
BRANCH = info.get("default_branch") or "main"
files = [p.decode("utf-8") for p in subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT).split(b"\0") if p]
print(f"{OWNER}/{REPO} · 分支 {BRANCH} · 文件 {len(files)}")

tree = []
for i, rel in enumerate(files, 1):
    blob = req(
        "POST",
        f"{API}/git/blobs",
        {"content": base64.b64encode((ROOT / rel).read_bytes()).decode(), "encoding": "base64"},
    )
    tree.append({"path": rel.replace("\\", "/"), "mode": "100644", "type": "blob", "sha": blob["sha"]})
    if i % 25 == 0 or i == len(files):
        print(f"  blob {i}/{len(files)}")

t = req("POST", f"{API}/git/trees", {"tree": tree})  # 注意：不带 base_tree
c = req("POST", f"{API}/git/commits", {"message": "LivePhoto 浏览器 v0.1.0", "tree": t["sha"]})  # 注意：不带 parents
req("PATCH", f"{API}/git/refs/heads/{BRANCH}", {"sha": c["sha"], "force": True})
print(f"历史已重写为单提交 {c['sha'][:7]}")
