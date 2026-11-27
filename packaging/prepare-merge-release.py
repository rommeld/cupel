"""Prepare a tag for a merged GitHub PR without modifying a branch or pushing."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


def git(*args):
    return subprocess.check_output(["git", *args], text=True)


def release_bump(pages, merged_at):
    directives = {
        comment["body"].strip()
        for page in pages
        for comment in page
        if comment["author_association"] in {"OWNER", "MEMBER", "COLLABORATOR"}
        and comment["updated_at"] <= merged_at
    }
    if "Changelog: added" in directives:
        return "minor"
    if directives & {"Changelog: fixed", "Changelog: refact"}:
        return "patch"
    return None


def stable_tags():
    fields = "--format=%(refname:lstrip=2) %(objecttype) %(contents:subject)"
    for line in git("tag", "--list", "v*", fields).splitlines():
        tag, kind, subject = line.split(" ", 2)
        if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
            version = tuple(map(int, tag[1:].split(".")))
            yield version, tag, subject if kind == "tag" else ""


def is_ancestor(ancestor, descendant):
    result = subprocess.run(["git", "merge-base", "--is-ancestor", ancestor, descendant])
    if result.returncode == 1:
        return False
    result.check_returncode()
    return True


def prepare(pr, repo):
    marker = f"Release PR #{pr['number']}"
    tags = list(stable_tags())
    for _, tag, subject in tags:
        if subject == marker:
            return tag

    response = subprocess.check_output([
        "gh", "api", "--paginate", "--slurp",
        f"repos/{repo}/issues/{pr['number']}/comments?per_page=100",
    ])
    bump = release_bump(json.loads(response), pr["merged_at"])
    if bump is None:
        print("No eligible changelog comment; skipping release", file=sys.stderr)
        return ""

    if tags:
        (major, minor, patch), previous, _ = max(tags)
        if is_ancestor("HEAD", previous):
            print(f"Merge already included in {previous}; skipping release", file=sys.stderr)
            return ""
        if not is_ancestor(previous, "HEAD"):
            Path("CHANGELOG.md").write_bytes(subprocess.check_output([
                "git", "show", f"{previous}:CHANGELOG.md",
            ]))
    else:
        previous = ""
        workspace = tomllib.loads(Path("Cargo.toml").read_text())["workspace"]
        major, minor, patch = map(int, workspace["package"]["version"].split("."))

    tag = f"v{major}.{minor + 1}.0" if bump == "minor" else f"v{major}.{minor}.{patch + 1}"
    script = Path(__file__).with_name("prepare-release.sh")
    git("switch", "--detach")
    subprocess.run(["sh", str(script), tag, previous], check=True, stdout=sys.stderr)
    git("add", "--", "Cargo.toml", "Cargo.lock", "CHANGELOG.md")
    git("commit", "-m", f"changelog: {tag}")
    git("tag", "-a", tag, "-m", marker)
    return tag


if __name__ == "__main__":
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    print(prepare(event["pull_request"], os.environ["GITHUB_REPOSITORY"]))
