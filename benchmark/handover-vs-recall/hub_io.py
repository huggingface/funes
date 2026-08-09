#!/usr/bin/env python3
"""Minimal HF Hub dataset I/O with only the Python standard library.

A fallback for hosts without the `hf` CLI / `huggingface_hub`. Covers exactly what
run.sh needs: check a file exists, download a subtree (a bundle), commit files.

  hub_io.py exists   <repo> <path>                 # exit 0 if the file is on the repo
  hub_io.py download <repo> <subpath> <local_dir>  # mirror <subpath> under <local_dir>
  hub_io.py upload   <repo> <local_dir> <dest>     # commit <local_dir>/* to <dest>/  [--exclude GLOB ...]

Auth: HF_TOKEN / HUGGING_FACE_HUB_TOKEN env, else ~/.cache/huggingface/token.
Repos are datasets; the default revision is main.
"""
import base64, fnmatch, json, os, sys, urllib.request, urllib.error

API = "https://huggingface.co/api/datasets"
RESOLVE = "https://huggingface.co/datasets/{repo}/resolve/main/{path}"


def token():
    t = os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    if t:
        return t.strip()
    p = os.path.expanduser("~/.cache/huggingface/token")
    return open(p).read().strip() if os.path.exists(p) else None


def _req(url, data=None, method="GET", ctype=None):
    h = {}
    tok = token()
    if tok:
        h["Authorization"] = f"Bearer {tok}"
    if ctype:
        h["Content-Type"] = ctype
    return urllib.request.Request(url, data=data, method=method, headers=h)


def tree(repo, subpath=""):
    # Recursive listing under subpath; returns the file entries.
    url = f"{API}/{repo}/tree/main/{subpath}".rstrip("/") + "?recursive=true"
    with urllib.request.urlopen(_req(url)) as r:
        return [e for e in json.load(r) if e.get("type") == "file"]


def cmd_exists(repo, path):
    try:
        with urllib.request.urlopen(_req(RESOLVE.format(repo=repo, path=path), method="HEAD")):
            return 0
    except urllib.error.HTTPError as e:
        return 0 if e.code in (200, 302) else 1
    except urllib.error.URLError:
        return 1


def cmd_download(repo, subpath, local_dir):
    files = tree(repo, subpath)
    for e in files:
        rel = e["path"]
        dst = os.path.join(local_dir, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        with urllib.request.urlopen(_req(RESOLVE.format(repo=repo, path=rel))) as r:
            data = r.read()
        with open(dst, "wb") as f:
            f.write(data)
    print(f"downloaded {len(files)} files from {repo}:{subpath} -> {local_dir}", file=sys.stderr)
    return 0


def cmd_upload(repo, local_dir, dest, excludes):
    ops = [{"key": "header", "value": {"summary": f"add {dest}"}}]
    n = 0
    for root, _, names in os.walk(local_dir):
        for name in sorted(names):
            full = os.path.join(root, name)
            rel = os.path.relpath(full, local_dir)
            if any(fnmatch.fnmatch(rel, g) or fnmatch.fnmatch(name, g) for g in excludes):
                continue
            with open(full, "rb") as f:
                content = base64.b64encode(f.read()).decode()
            ops.append({"key": "file", "value": {
                "path": f"{dest}/{rel}".replace("\\", "/"),
                "encoding": "base64", "content": content}})
            n += 1
    body = ("\n".join(json.dumps(o) for o in ops) + "\n").encode()
    url = f"https://huggingface.co/api/datasets/{repo}/commit/main"
    try:
        with urllib.request.urlopen(_req(url, data=body, method="POST", ctype="application/x-ndjson")) as r:
            json.load(r)
    except urllib.error.HTTPError as e:
        print(f"upload failed HTTP {e.code}: {e.read().decode()[:300]}", file=sys.stderr)
        return 1
    print(f"committed {n} files to {repo}:{dest}", file=sys.stderr)
    return 0


def main(argv):
    if not argv:
        print(__doc__)
        return 2
    op, rest = argv[0], argv[1:]
    if op == "exists":
        return cmd_exists(rest[0], rest[1])
    if op == "download":
        return cmd_download(rest[0], rest[1], rest[2])
    if op == "upload":
        excludes, pos, i = [], [], 0
        while i < len(rest):
            if rest[i] == "--exclude":
                excludes.append(rest[i + 1]); i += 2
            else:
                pos.append(rest[i]); i += 1
        return cmd_upload(pos[0], pos[1], pos[2], excludes)
    print(f"unknown subcommand: {op}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
