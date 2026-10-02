#!/usr/bin/env python3
"""显式真实 OSS CLI 验收；保留成功样本，凭证文件只在父进程内读取。"""

import argparse
import csv
import json
import os
import re
from pathlib import Path
import secrets
import subprocess
import time
from acceptance import digest, events, receipt

REPO = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--credentials", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument(
        "--prefix", default="samples/", help="isolated sample prefix for this run"
    )
    args = parser.parse_args()
    if not re.fullmatch(r"(?:[A-Za-z0-9_-]+/)+", args.prefix):
        parser.error("prefix must contain plain relative directory names")
    if not args.bucket.startswith("waybill-test-"):
        parser.error("requires a dedicated waybill-test- bucket")
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=True, mode=0o700)
    with args.credentials.open(newline="", encoding="utf-8-sig") as file:
        row = next(csv.DictReader(file))
    env = os.environ.copy()
    env.update(
        OSS_ACCESS_KEY_ID=row["AccessKey ID"],
        OSS_ACCESS_KEY_SECRET=row["AccessKey Secret"],
        WAYBILL_STATE_DIR=str(work / "state"),
        OSS_LIVE_BUCKET=args.bucket,
        OSS_LIVE_RUN=secrets.token_hex(8),
    )
    config = work / "object.json"
    config.write_text(
        json.dumps(
            {
                "backend": "oss",
                "namespace": args.bucket,
                "conditional_writes": True,
                "options": {
                    "bucket": args.bucket,
                    "endpoint": "https://oss-cn-shenzhen.aliyuncs.com",
                    "access_key_id": "${OSS_ACCESS_KEY_ID}",
                    "access_key_secret": "${OSS_ACCESS_KEY_SECRET}",
                },
            },
            indent=2,
        )
    )
    config.chmod(0o600)
    report = {
        "bucket": args.bucket,
        "backend": "oss",
        "region": "cn-shenzhen",
        "prefix": args.prefix,
        "checks": [],
    }

    def passed(message):
        report["checks"].append(message)
        (work / "results.json").write_text(
            json.dumps(report, indent=2, ensure_ascii=False)
        )
        print("PASS: " + message, flush=True)

    def run(argv, expected=0):
        result = subprocess.run(
            argv, env=env, cwd=REPO, text=True, capture_output=True, timeout=900
        )
        if result.returncode != expected:
            diagnostic = f"{Path(argv[0]).name} exited {result.returncode}: {result.stdout} {result.stderr}"
            for credential in (row["AccessKey ID"], row["AccessKey Secret"]):
                if credential:
                    diagnostic = diagnostic.replace(credential, "[REDACTED]")
            raise RuntimeError(diagnostic)
        return result.stdout

    def wb(*argv, expected=0):
        return run([str(REPO / "target/debug/wb"), "--json", *argv], expected)

    login = json.loads(
        wb("login", "--account", "oss-test", "object", "--config", str(config))
    )
    assert login["capabilities"] == {"range_download": True, "stream_upload": True}
    wb("drive", "add", "oss-test", "--provider", "object", "--account", "oss-test")
    passed("OSS config import and directory access")
    source = work / "source.bin"
    if not source.exists():
        source.write_bytes(bytes(range(256)) * (17 * 1024 * 1024 // 256))
    first = receipt(
        wb(
            "put",
            str(source),
            "--to",
            args.prefix,
            "--operation",
            "oss-17m",
            "--no-tui",
        )
    )
    repeated = receipt(
        wb(
            "put",
            str(source),
            "--to",
            args.prefix,
            "--operation",
            "oss-17m",
            "--no-tui",
        )
    )
    assert first == repeated
    download = work / "downloaded.bin"
    wb("get", args.prefix + "source.bin", str(download), "--no-tui")
    assert digest(source) == digest(download)
    report["source_sha256"] = digest(source)
    passed(
        "17 MiB multipart, complete conditional writer publication, download checksum and receipt reuse"
    )
    listing = json.loads(wb("list", args.prefix, "--no-tui"))
    assert any(row["name"] == "source.bin" for row in listing)
    assert all(not row["name"].startswith(".waybill-") for row in listing)
    wb("drive", "root", "oss-test", args.prefix)
    assert any(
        row["id"] == "source.bin" for row in json.loads(wb("list", "/", "--no-tui"))
    )
    wb("drive", "root", "oss-test", "/")
    passed("prefix directories, hidden staging and selected roots")
    changed = work / "conflict.bin"
    if not changed.exists():
        changed.write_bytes(b"different content")
    failed = events(
        wb(
            "put",
            str(changed),
            "--to",
            args.prefix + "source.bin",
            "--operation",
            "oss-conflict",
            "--no-tui",
            expected=1,
        )
    )
    assert any("Conflict" in item.get("message", "") for item in failed)
    suffix = receipt(
        wb(
            "put",
            str(changed),
            "--to",
            args.prefix + "source.bin",
            "--operation",
            "oss-suffix",
            "--conflict",
            "operation-suffix",
            "--no-tui",
        )
    )
    assert suffix["object"] != args.prefix + "source.bin"
    empty = work / "空 文件.txt"
    if not empty.exists():
        empty.touch()
    wb("put", str(empty), "--to", args.prefix, "--no-tui")
    wb("get", args.prefix + "空 文件.txt", str(work / "empty-download"), "--no-tui")
    assert (work / "empty-download").stat().st_size == 0
    passed("conflict rejection, suffix, Unicode and empty files")
    print(
        run(
            [
                "cargo",
                "test",
                "-p",
                "waybill-service-opendal",
                "--test",
                "oss_live",
                "--locked",
                "--",
                "--ignored",
                "--nocapture",
            ]
        ),
        flush=True,
    )
    passed("live publication race preserves competing object")
    large = work / "interrupted.bin"
    with large.open("wb") as file:
        for _ in range(128):
            file.write(bytes(range(256)) * 4096)
    operation = "oss-interruption"
    argv = [
        str(REPO / "target/debug/wb"),
        "--json",
        "put",
        str(large),
        "--to",
        args.prefix + "interrupted.bin",
        "--operation",
        operation,
        "--no-tui",
    ]
    with (work / "interruption-output.jsonl").open("w") as output:
        process = subprocess.Popen(
            argv, env=env, stdout=output, stderr=subprocess.PIPE, cwd=REPO
        )
        try:
            deadline = time.monotonic() + 120
            killed = False
            while process.poll() is None:
                for path in (work / "state/checkpoints").glob("*.json"):
                    try:
                        saved = json.loads(path.read_text())
                        flow = saved.get("flow", {})
                        if flow.get("intent", {}).get("operation") != operation:
                            continue
                        payload = flow.get("driver", {}).get("payload")
                        if (
                            payload
                            and json.loads(bytes(payload))["phase"]["phase"]
                            == "writing"
                        ):
                            process.kill()
                            killed = True
                            break
                    except (ValueError, OSError, KeyError):
                        pass
                if killed:
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError("persistent write intent not observed")
                time.sleep(0.005)
            process.communicate(timeout=10)
            assert killed, "upload finished before interruption injection"
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=10)
    failed = events(
        wb(
            "put",
            str(large),
            "--to",
            args.prefix + "interrupted.bin",
            "--operation",
            operation,
            "--no-tui",
            expected=1,
        )
    )
    assert any("SessionExpired" in item.get("message", "") for item in failed)
    wb(
        "put",
        str(large),
        "--to",
        args.prefix + "interrupted.bin",
        "--operation",
        operation,
        "--allow-restart",
        "--no-tui",
    )
    recovered = work / "recovered.bin"
    wb("get", args.prefix + "interrupted.bin", str(recovered), "--no-tui")
    assert digest(large) == digest(recovered)
    report["interrupted_sha256"] = digest(large)
    passed(
        "SIGKILL, default restart refusal, explicit 128 MiB restart and download checksum"
    )
    print(
        "Successful samples retained; clean scratch, .waybill- objects and unfinished multipart with scoped administration.",
        flush=True,
    )


if __name__ == "__main__":
    main()
