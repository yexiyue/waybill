#!/usr/bin/env python3
"""清理专用 OSS 验收 bucket 的暂存和 scratch；保留 samples 下的成功样本。"""

import argparse
import csv
import json
import os
import re
import subprocess
from pathlib import Path


def items(value):
    if isinstance(value, dict):
        return [value]
    return value if isinstance(value, list) else []


def disposable(key):
    return (
        key.startswith("scratch/")
        or re.fullmatch(r"samples/\.waybill-[0-9a-f]{32}\.part", key) is not None
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--credentials", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--ossutil", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"waybill-test-\d{8}-[0-9a-f]{8}", args.bucket):
        parser.error("dedicated acceptance bucket required")
    with args.credentials.open(newline="", encoding="utf-8-sig") as file:
        row = next(csv.DictReader(file))
    env = os.environ.copy()
    env.update(
        OSS_ACCESS_KEY_ID=row["AccessKey ID"],
        OSS_ACCESS_KEY_SECRET=row["AccessKey Secret"],
        OSS_REGION="cn-shenzhen",
        OSS_ENDPOINT="https://oss-cn-shenzhen.aliyuncs.com",
    )

    def run(*argv):
        try:
            result = subprocess.run(
                [str(args.ossutil), *argv],
                env=env,
                text=True,
                capture_output=True,
                timeout=120,
            )
        except subprocess.TimeoutExpired:
            raise RuntimeError("OSS cleanup timed out; session IDs omitted") from None
        if result.returncode:
            raise RuntimeError(
                "OSS cleanup operation failed; no credentials or session IDs emitted"
            )
        return result.stdout

    def api(*argv):
        text = run("api", *argv, "--output-format", "json").strip()
        return json.JSONDecoder().raw_decode(text[text.index("{") :])[0]

    data = api("list-objects-v2", "--bucket", args.bucket)
    if str(data.get("IsTruncated", "")).lower() == "true":
        raise RuntimeError(
            "test bucket listing exceeded one page; no cleanup performed"
        )
    remove = []
    for item in items(data.get("Contents")):
        key = item["Key"]
        if disposable(key):
            remove.append(key)
    uploads = api("list-multipart-uploads", "--bucket", args.bucket)
    if str(uploads.get("IsTruncated", "")).lower() == "true":
        raise RuntimeError("multipart listing exceeded one page; no cleanup performed")
    pending = items(uploads.get("Upload"))
    if any(not disposable(item["Key"]) for item in pending):
        raise RuntimeError("unexpected multipart key; no cleanup performed")
    for key in remove:
        run("rm", f"oss://{args.bucket}/{key}", "--force")
    for upload in pending:
        # upload ID 仅作为 SDK 参数使用，不输出或写入验收记录。
        api(
            "abort-multipart-upload",
            "--bucket",
            args.bucket,
            "--key",
            upload["Key"],
            "--upload-id",
            upload["UploadId"],
        )
    remaining = api("list-objects-v2", "--bucket", args.bucket)
    objects = items(remaining.get("Contents"))
    assert all(
        item["Key"].startswith("samples/")
        and not item["Key"].split("/")[-1].startswith(".waybill-")
        for item in objects
    )
    assert not items(
        api("list-multipart-uploads", "--bucket", args.bucket).get("Upload")
    )
    report_path = args.work_dir / "results.json"
    report = json.loads(report_path.read_text())
    report["cleanup"] = {
        "removed_objects": len(remove),
        "aborted_multipart": len(pending),
        "remaining_multipart": 0,
        "retained_samples": [
            {"key": item["Key"], "size": int(item["Size"])} for item in objects
        ],
    }
    report_path.write_text(json.dumps(report, indent=2, ensure_ascii=False))
    print(json.dumps(report["cleanup"], ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
