#!/usr/bin/env python3
"""Build the offline phone catalog from checked-out PUBLIC chrome-use packs.

No Chrome profile, cookie, private adapter cache, CLI or executable code is read.
Use the same parsing and portability rules as PhoneSiteCatalogDownloader.
"""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess

REPOSITORIES = ("epiral/bb-sites", "leeguooooo/chrome-use-sites")
NODE = re.compile(r"\b(?:require\s*\(|process\s*\.|Buffer\s*\.|module\s*\.|exports\s*\.|import\s|export\s|chrome\s*\.)")
SPEC = re.compile(r"^[A-Za-z0-9_-]+/[A-Za-z0-9_-]+$")
SECRET = re.compile(r"(?:github_pat_[A-Za-z0-9_]{30,}|ghp_[A-Za-z0-9]{30,}|AIza[A-Za-z0-9_-]{30,}|sk-[A-Za-z0-9]{30,})")

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--community", type=pathlib.Path)
    parser.add_argument("--official", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--notices", type=pathlib.Path, required=True)
    args = parser.parse_args()
    adapters, unsupported, revisions, notices = {}, set(), {}, []
    for repository, root in zip(REPOSITORIES, (args.community, args.official)):
        if root is None:
            continue
        remote = subprocess.check_output(["git", "-C", str(root), "remote", "get-url", "origin"], text=True).strip()
        if remote.removesuffix(".git") != "https://github.com/" + repository:
            raise SystemExit("Use the public official/community repository, not a private adapter cache")
        commit = subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip()
        revisions[repository] = commit
        license_file = next((root / name for name in ("LICENSE", "LICENSE.md", "LICENSE.txt") if (root / name).exists()), None)
        if license_file is None:
            raise SystemExit("Pack license must be present before bundling: " + repository)
        notices.append(repository + " @ " + commit + "\n" + license_file.read_text())
        tracked = subprocess.check_output(["git", "-C", str(root), "ls-files", "*.js"], text=True).splitlines()
        for relative in tracked:
            spec = relative.removesuffix(".js")
            if not SPEC.fullmatch(spec):
                continue
            path = root / relative
            if path.is_symlink():
                continue
            data = path.read_bytes()
            if len(data) > 512_000:
                continue
            text = data.decode("utf-8")
            if SECRET.search(text):
                raise SystemExit("Potential embedded credential; file not bundled: " + repository + "/" + relative)
            match = re.search(r"/\* @meta(.*?)\*/", text, re.S)
            if not match:
                continue
            try:
                meta = json.loads(match.group(1))
            except json.JSONDecodeError:
                continue
            source = text[match.end():].strip()
            adapters.pop(spec, None)
            unsupported.discard(spec)
            domain = meta.get("domain", "")
            file_arg = any(value.get("type") == "file" for value in meta.get("args", {}).values() if isinstance(value, dict))
            if not source.startswith("async function") or NODE.search(source) or file_arg or meta.get("runtime") == "opencli" or ".setOn(" in source or not domain:
                unsupported.add(spec)
                continue
            adapters[spec] = dict(spec=spec, meta=meta, source=source,
                sha256=hashlib.sha256(source.encode()).hexdigest(),
                origin="https://github.com/" + repository + "/blob/" + commit + "/" + relative)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(dict(catalog=dict(chrome_use_sites=1, adapters=[adapters[key] for key in sorted(adapters)]),
        unsupported=sorted(unsupported), revisions=revisions), ensure_ascii=False, indent=2) + "\n")
    args.notices.write_text("Public site adapter attribution and licenses\n\n" + "\n\n".join(notices))
    print(json.dumps(dict(phone_adapters=len(adapters), unsupported=len(unsupported), revisions=revisions)))

if __name__ == "__main__":
    main()
