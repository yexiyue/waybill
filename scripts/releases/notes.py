#!/usr/bin/env python3
"""核对发布版本并生成精确 tag 范围的版本说明，不调用发布端口。"""

import argparse
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
TAG = re.compile(
    r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?"
)
START = "<!-- waybill-release:start -->"
END = "<!-- waybill-release:end -->"
REPOSITORY = "https://github.com/yexiyue/waybill"


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def check(tag, require_tag=False):
    if not TAG.fullmatch(tag):
        raise ValueError("release tag must be v<semver>, without build metadata")
    version = tag[1:]
    metadata = json.loads(
        command("cargo", "metadata", "--format-version", "1", "--no-deps", "--locked")
    )
    packages = [p for p in metadata["packages"] if p["publish"] != []]
    for package in packages:
        if package["version"] != version or package["publish"] != ["crates-io"]:
            raise ValueError(
                f"{package['name']} version or publish registry does not match release"
            )
        for dependency in package["dependencies"]:
            if dependency.get("path") and dependency["req"] not in (
                f"^{version}",
                f"={version}",
            ):
                raise ValueError(
                    f"{package['name']}: missing matching version on path dependency {dependency['name']}"
                )
    description = ROOT / "docs" / "releases" / f"{tag}.md"
    if not description.is_file() or not description.read_text().strip():
        raise ValueError(
            f"missing version description: {description.relative_to(ROOT)}"
        )
    if require_tag and command(
        "git", "rev-parse", f"refs/tags/{tag}^{{commit}}"
    ) != command("git", "rev-parse", "HEAD"):
        raise ValueError("checked-out commit does not match release tag")
    return description


def render(tag, output):
    description = check(tag, require_tag=True)
    tags = command(
        "git", "tag", "--merged", tag, "--sort=-version:refname"
    ).splitlines()
    previous = next(
        (value for value in tags if value != tag and TAG.fullmatch(value)), None
    )
    revision = f"{previous}..{tag}" if previous else tag
    commits = command("git", "log", "--format=%H%x09%s", revision).splitlines()
    lines = [description.read_text().strip(), "", "## Git log", ""]
    if previous:
        lines += [f"[完整差异]({REPOSITORY}/compare/{previous}...{tag})", ""]
    else:
        lines += ["首次发布：包含截至本 tag 的完整提交历史。", ""]
    for commit in commits:
        sha, subject = commit.split("\t", 1)
        subject = re.sub(r"([\\`*_{}\[\]<>])", r"\\\1", subject)
        lines.append(f"- [{sha[:7]}]({REPOSITORY}/commit/{sha}) {subject}")
    # 保留用户编写的 Release 正文，仅替换本工作流管理的区段；重跑不重复追加。
    existing = (
        json.loads(command("gh", "release", "view", tag, "--json", "body"))["body"]
        or ""
    )
    if START in existing or END in existing:
        if (
            existing.count(START) != 1
            or existing.count(END) != 1
            or existing.index(END) < existing.index(START)
        ):
            raise ValueError("malformed generated release notes block")
        existing = re.sub(
            re.escape(START) + r".*?" + re.escape(END), "", existing, flags=re.S
        ).strip()
    generated = START + "\n" + "\n".join(lines) + "\n" + END
    output.write_text((existing + "\n\n" if existing else "") + generated + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    verify = sub.add_parser("check")
    verify.add_argument("tag")
    verify.add_argument("--require-tag", action="store_true")
    notes = sub.add_parser("render")
    notes.add_argument("tag")
    notes.add_argument("output", type=Path)
    args = parser.parse_args()
    if args.command == "check":
        check(args.tag, args.require_tag)
    else:
        render(args.tag, args.output)


if __name__ == "__main__":
    main()
