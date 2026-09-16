"""用 GitHub HTTP API 推送仓库内容（绕过 git push）。

为什么不用 git push：本机 git 直连 github.com:443 被阻断（连接被重置/超时），
而 `gh`（Go TLS 栈）与 Python urllib 走 API 是通的。

流程：
  1. 空仓库上用 Contents API 建第一个文件 —— 它会自动创建默认分支；
     （Git Data API 在没有任何提交的空仓库上会返回 409）
  2. 之后用 Git Data API 把其余文件一次性做成第二个提交。

用法： python tools/gh-push.py <owner> <repo>
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
BRANCH = "master"

ROOT = Path(__file__).resolve().parent.parent
API = f"https://api.github.com/repos/{OWNER}/{REPO}"
token = subprocess.check_output(["gh", "auth", "token"], text=True).strip()


def req(method: str, url: str, data=None, ok=(200, 201)):
    body = json.dumps(data).encode() if data is not None else None
    r = urllib.request.Request(
        url,
        data=body,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "User-Agent": "livephoto-push",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(r, timeout=90) as resp:
            raw = resp.read().decode()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        if e.code in ok:
            return {}
        print(f"  HTTP {e.code} {method} {url}: {e.read().decode()[:300]}", file=sys.stderr)
        raise


def git_files():
    """列出已跟踪文件。

    必须用 -z：默认的 ls-files 会对非 ASCII 路径做引号+八进制转义
    （core.quotepath），中文文件名会变成 "docs\\M0-\\345..." 这种没法直接用的字符串。
    """
    raw = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT)
    return [p.decode("utf-8") for p in raw.split(b"\0") if p]


files = git_files()
print(f"仓库 {OWNER}/{REPO} · 待推送 {len(files)} 个文件")

# 1) 空仓库：用 Contents API 建分支（Git Data API 在无提交的仓库上会 409）
# 1) 分支名必须从 API 读：新仓库的默认分支是 main，不是 master。
#    （踩过：写死 master 会误判"分支不存在"，于是又去建已存在的文件 → 422 sha wasn't supplied）
info = req("GET", API)
BRANCH = info.get("default_branch") or "master"
print(f"默认分支：{BRANCH}")

branch_exists = True
try:
    req("GET", f"{API}/git/ref/heads/{BRANCH}")
except urllib.error.HTTPError:
    branch_exists = False

boot = ".gitignore" if ".gitignore" in files else files[0]
if not branch_exists:
    print(f"引导分支：{boot}")
    req(
        "PUT",
        f"{API}/contents/{boot}",
        {
            "message": "chore: bootstrap repository",
            "content": base64.b64encode((ROOT / boot).read_bytes()).decode(),
        },
    )
else:
    print("分支已存在，跳过引导")

rest = [f for f in files if f != boot]
print(f"其余 {len(rest)} 个文件走 Git Data API")

# 2) 建 blob
tree = []
for i, rel in enumerate(rest, 1):
    blob = req(
        "POST",
        f"{API}/git/blobs",
        {"content": base64.b64encode((ROOT / rel).read_bytes()).decode(), "encoding": "base64"},
    )
    tree.append({"path": rel.replace("\\", "/"), "mode": "100644", "type": "blob", "sha": blob["sha"]})
    if i % 25 == 0 or i == len(rest):
        print(f"  已上传 {i}/{len(rest)}")

# 3) tree（带 base_tree，保留引导提交的 .gitignore）
head = req("GET", f"{API}/git/ref/heads/{BRANCH}")
base_sha = head["object"]["sha"]
base_commit = req("GET", f"{API}/git/commits/{base_sha}")
t = req("POST", f"{API}/git/trees", {"base_tree": base_commit["tree"]["sha"], "tree": tree})

# 4) commit（提交信息是中文，必须显式按 UTF-8 解码 —— text=True 会用系统 GBK 而报错）
msg = (
    subprocess.check_output(["git", "log", "-1", "--pretty=%B"], cwd=ROOT)
    .decode("utf-8", "replace")
    .strip()
)
c = req("POST", f"{API}/git/commits", {"message": msg, "tree": t["sha"], "parents": [base_sha]})

# 5) 更新 ref
req("PATCH", f"{API}/git/refs/heads/{BRANCH}", {"sha": c["sha"], "force": True})

print(f"完成：https://github.com/{OWNER}/{REPO}")
