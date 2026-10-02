#!/usr/bin/env python3
"""本地 RustFS/S3 CLI 验收；只创建临时容器、bucket 与宿主状态目录。"""

import hashlib
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import time
import urllib.request

REPO = Path(__file__).resolve().parents[2]
IMAGE = "rustfs/rustfs@sha256:8cc9801755448b71a786705ce76692c77e14936cccd87cf2fc31842e58f4d1ff"
MC = "minio/mc@sha256:a7fe349ef4bd8521fb8497f55c6042871b2ae640607cf99d9bede5e9bdf11727"
WB = Path(os.environ.get("WAYBILL_TEST_BIN", REPO / "target/debug/wb"))


def command(args, env=None, expected=0):
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=120)
    if result.returncode != expected:
        raise RuntimeError(
            f"command failed: {args[0]} (exit {result.returncode})\n{result.stderr}"
        )
    return result.stdout


def digest(path):
    with path.open("rb") as file:
        checksum = hashlib.sha256()
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            checksum.update(chunk)
        return checksum.hexdigest()


def events(text):
    return [json.loads(line) for line in text.splitlines()]


def receipt(text):
    return next(event for event in events(text) if event.get("type") == "completed")


def main():
    name = "waybill-object-test-" + secrets.token_hex(4)
    env = os.environ.copy()
    env["WB_OBJECT_ACCESS_KEY"] = "waybill-test-" + secrets.token_hex(4)
    env["WB_OBJECT_SECRET_KEY"] = secrets.token_hex(24)
    container_env = env.copy()
    container_env["RUSTFS_ACCESS_KEY"] = env["WB_OBJECT_ACCESS_KEY"]
    container_env["RUSTFS_SECRET_KEY"] = env["WB_OBJECT_SECRET_KEY"]
    command(
        [
            "docker",
            "run",
            "--detach",
            "--name",
            name,
            "--publish",
            "127.0.0.1::9000",
            "--env",
            "RUSTFS_ACCESS_KEY",
            "--env",
            "RUSTFS_SECRET_KEY",
            "--env",
            "RUSTFS_CONSOLE_ENABLE=false",
            IMAGE,
        ],
        container_env,
    )
    try:
        address = command(["docker", "port", name, "9000"]).strip()
        endpoint = "http://" + address
        deadline = time.monotonic() + 60
        while True:
            try:
                with urllib.request.urlopen(
                    endpoint + "/health", timeout=2
                ) as response:
                    if response.status == 200:
                        break
            except OSError:
                if time.monotonic() > deadline:
                    raise RuntimeError("RustFS did not become ready")
                time.sleep(0.2)
        command(
            [
                "docker",
                "run",
                "--rm",
                "--network",
                "container:" + name,
                "--env",
                "WB_OBJECT_ACCESS_KEY",
                "--env",
                "WB_OBJECT_SECRET_KEY",
                "--entrypoint",
                "/bin/sh",
                MC,
                "-c",
                'mc alias set test http://127.0.0.1:9000 "$WB_OBJECT_ACCESS_KEY" "$WB_OBJECT_SECRET_KEY" >/dev/null && mc mb test/waybill-test',
            ],
            env,
        )
        with tempfile.TemporaryDirectory(
            prefix="waybill-object-acceptance-"
        ) as temporary:
            work = Path(temporary)
            env["WAYBILL_STATE_DIR"] = str(work / "state")
            config = work / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "backend": "s3",
                        "namespace": name,
                        "conditional_writes": True,
                        "options": {
                            "endpoint": endpoint,
                            "bucket": "waybill-test",
                            "region": "us-east-1",
                            "access_key_id": "${WB_OBJECT_ACCESS_KEY}",
                            "secret_access_key": "${WB_OBJECT_SECRET_KEY}",
                            "disable_config_load": "true",
                        },
                    }
                )
            )

            def wb(*args, expected=0):
                return command([str(WB), "--json", *args], env, expected)

            wb("login", "--account", "local", "object", "--config", str(config))
            wb("drive", "add", "local", "--provider", "object", "--account", "local")
            source = work / "source.bin"
            source.write_bytes(bytes(range(256)) * (17 * 1024 * 1024 // 256))
            first = receipt(wb("put", str(source), "--to", "backup/", "--no-tui"))
            second = receipt(wb("put", str(source), "--to", "backup/", "--no-tui"))
            assert first == second
            downloaded = work / "downloaded.bin"
            wb("get", "backup/source.bin", str(downloaded), "--no-tui")
            assert digest(source) == digest(downloaded)
            listing = json.loads(wb("list", "backup/", "--no-tui"))
            assert [row["name"] for row in listing] == ["source.bin"]
            wb("drive", "root", "local", "backup/")
            assert json.loads(wb("list", "/", "--no-tui"))[0]["id"] == "source.bin"
            wb("drive", "root", "local", "/")
            source.write_bytes(b"different content")
            failed = events(
                wb("put", str(source), "--to", "backup/", "--no-tui", expected=1)
            )
            assert any(
                e.get("type") == "failed" and "Conflict" in e.get("message", "")
                for e in failed
            )
            suffix = receipt(
                wb(
                    "put",
                    str(source),
                    "--to",
                    "backup/",
                    "--conflict",
                    "operation-suffix",
                    "--operation",
                    "docker-suffix",
                    "--no-tui",
                )
            )
            assert suffix["object"] != "backup/source.bin"
            empty = work / "空 文件.txt"
            empty.touch()
            wb("put", str(empty), "--to", "backup/", "--no-tui")
            wb("get", "backup/空 文件.txt", str(work / "empty-download"), "--no-tui")
            assert (work / "empty-download").stat().st_size == 0
            print(
                "PASS: config, multipart 17 MiB, download checksum, receipt reuse, listing, roots, conflicts, suffix, Unicode and empty files",
                flush=True,
            )

            # 根据落盘状态触发 SIGKILL，不依赖机器速度或 TUI 进度节流。
            large = work / "interrupted.bin"
            with large.open("wb") as file:
                for _ in range(128):
                    file.write(bytes(range(256)) * 4096)
            operation = "docker-interruption"
            args = [
                str(WB),
                "--json",
                "put",
                str(large),
                "--to",
                "backup/interrupted.bin",
                "--operation",
                operation,
                "--no-tui",
            ]
            with (work / "interruption-output.jsonl").open("w") as output:
                process = subprocess.Popen(
                    args, env=env, stdout=output, stderr=subprocess.PIPE
                )
                deadline = time.monotonic() + 30
                killed = False
                while process.poll() is None:
                    for path in (work / "state/checkpoints").glob("**/*.json"):
                        try:
                            saved = json.loads(path.read_text())
                            flow = saved.get("flow", {})
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
                        process.kill()
                        raise RuntimeError("did not observe persistent write intent")
                    time.sleep(0.005)
                process.communicate(timeout=10)
                assert killed, "upload completed before interruption was injected"
            failed = events(
                wb(
                    "put",
                    str(large),
                    "--to",
                    "backup/interrupted.bin",
                    "--operation",
                    operation,
                    "--no-tui",
                    expected=1,
                )
            )
            assert any("SessionExpired" in event.get("message", "") for event in failed)
            wb(
                "put",
                str(large),
                "--to",
                "backup/interrupted.bin",
                "--operation",
                operation,
                "--allow-restart",
                "--no-tui",
            )
            recovered = work / "recovered.bin"
            wb("get", "backup/interrupted.bin", str(recovered), "--no-tui")
            assert digest(large) == digest(recovered)
            print(
                "PASS: SIGKILL after durable write intent, default restart refusal, explicit whole-file restart, 128 MiB checksum",
                flush=True,
            )
    finally:
        command(["docker", "rm", "--force", "--volumes", name])
    print(
        "PASS: isolated state and credentials; test container and data removed",
        flush=True,
    )


if __name__ == "__main__":
    main()
