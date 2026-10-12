#!/usr/bin/env python3
"""Measure an authenticated Journey diagnostic download endpoint with curl."""

import argparse
import json
import re
import subprocess
import sys
import time
from urllib.parse import urlsplit

DEFAULT_BYTES = 256 * 1024 * 1024
MIB = 1024 * 1024
UNITS = {
    "": 1, "B": 1,
    "K": 1024, "KIB": 1024, "KB": 1000,
    "M": MIB, "MIB": MIB, "MB": 1000 * 1000,
    "G": 1024 ** 3, "GIB": 1024 ** 3, "GB": 1000 ** 3,
}
WRITE_OUT = ('{"http_code":%{http_code},"size_download":%{size_download},'
             '"time_total":%{time_total},"time_starttransfer":%{time_starttransfer},'
             '"speed_download":%{speed_download}}')


def parse_size(value):
    match = re.fullmatch(r"([0-9]+)([A-Za-z]*)", value)
    if not match or match.group(2).upper() not in UNITS:
        raise argparse.ArgumentTypeError("size must be a positive integer with a supported suffix")
    amount = int(match.group(1)) * UNITS[match.group(2).upper()]
    if amount < 1 or amount > 2 ** 64 - 1:
        raise argparse.ArgumentTypeError("size must fit in a positive unsigned 64-bit integer")
    return amount


def positive_int(value):
    try:
        number = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("concurrency must be positive") from error
    if number < 1:
        raise argparse.ArgumentTypeError("concurrency must be positive")
    return number


def curl_command(url, cookie_jar, head=False):
    command = ["curl", "--fail", "--silent", "--show-error"]
    if cookie_jar:
        command.extend(["--cookie", cookie_jar])
    command.extend(["--header", "Accept-Encoding: identity", "--output", "/dev/null",
                    "--write-out", WRITE_OUT])
    if head:
        command.append("--head")
    return command + [url]


def run(args):
    url = f"{args.url}?bytes={args.bytes}"
    preparation = subprocess.run(curl_command(url, args.cookie_jar, True),
                                 capture_output=True, text=True, check=False)
    try:
        head_metrics = json.loads(preparation.stdout)
    except (ValueError, TypeError):
        head_metrics = {}
    if preparation.returncode or head_metrics.get("http_code") != 200:
        raise RuntimeError(f"preparation HEAD failed: curl={preparation.returncode}, "
                           f"HTTP={head_metrics.get('http_code')}, {preparation.stderr.strip()}")

    started = time.monotonic()
    processes = [subprocess.Popen(curl_command(url, args.cookie_jar),
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                 for _ in range(args.concurrency)]
    transfers = []
    for process in processes:
        stdout, stderr = process.communicate()
        try:
            metrics = json.loads(stdout)
        except (ValueError, TypeError):
            metrics = {}
        byte_count = metrics.get("size_download", 0)
        successful = (process.returncode == 0 and metrics.get("http_code") == 200
                      and byte_count == args.bytes)
        seconds = metrics.get("time_total", 0)
        transfers.append({**metrics, "curl_exit_status": process.returncode,
                          "end_to_end_bytes_per_second": byte_count / seconds if seconds else 0,
                          "end_to_end_mib_per_second": byte_count / seconds / MIB if seconds else 0,
                          "success": successful, "error": stderr.strip() if stderr else None})
    elapsed = time.monotonic() - started
    total = sum(transfer.get("size_download", 0) for transfer in transfers)
    result = {"url": args.url, "requested_bytes_per_transfer": args.bytes,
              "concurrency": args.concurrency, "transfers": transfers,
              "batch_elapsed_seconds": elapsed, "total_bytes": total,
              "aggregate_bytes_per_second": total / elapsed if elapsed else 0,
              "aggregate_mib_per_second": total / elapsed / MIB if elapsed else 0,
              "success": all(transfer["success"] for transfer in transfers)}
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(f"{args.url}: {len(transfers)} transfers, {total} bytes in {elapsed:.3f}s; "
              f"aggregate {result['aggregate_mib_per_second']:.2f} MiB/s")
        for index, transfer in enumerate(transfers, 1):
            print(f"  {index}: HTTP {transfer.get('http_code', '?')}, "
                  f"{transfer.get('size_download', 0)} bytes, "
                  f"response start {transfer.get('time_starttransfer', 0):.3f}s, "
                  f"total {transfer.get('time_total', 0):.3f}s, "
                  f"curl speed {transfer.get('speed_download', 0):.0f} B/s, "
                  f"{'ok' if transfer['success'] else 'FAILED'}")
    return 0 if result["success"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url", help="website diagnostic endpoint URL without a query string")
    parser.add_argument("cookie_jar", nargs="?", help="existing admin cookie jar for /api/perftest URLs")
    parser.add_argument("--bytes", type=parse_size, default=DEFAULT_BYTES)
    parser.add_argument("--concurrency", type=positive_int, default=1)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    parts = urlsplit(args.url)
    if parts.scheme not in ("http", "https") or not parts.netloc or parts.query or parts.fragment:
        parser.error("url must be an HTTP(S) endpoint without a query or fragment")
    try:
        return run(args)
    except RuntimeError as error:
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
