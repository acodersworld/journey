#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("performance_curl", Path(__file__).with_name("performance-curl.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class FakeProcess:
    def __init__(self, bytes_downloaded, status=200, exit_status=0):
        self.returncode = exit_status
        self.bytes_downloaded = bytes_downloaded
        self.status = status

    def communicate(self):
        return json.dumps({"http_code": self.status, "size_download": self.bytes_downloaded,
                           "time_total": 2.0, "time_starttransfer": 0.2,
                           "speed_download": self.bytes_downloaded / 1.8}), ""


class RunnerTests(unittest.TestCase):
    def test_capability_url_needs_no_cookie_jar(self):
        command = runner.curl_command("https://example.test/perftest/TOKEN/site?bytes=1", None)
        self.assertNotIn("--cookie", command)
        self.assertIn("Accept-Encoding: identity", command)

    def test_size_conventions(self):
        self.assertEqual(runner.parse_size("512K"), 524288)
        self.assertEqual(runner.parse_size("10mb"), 10_000_000)
        self.assertEqual(runner.parse_size("256MiB"), 268435456)
        for invalid in ("0", "1.5M", "1T", "18446744073709551616"):
            with self.assertRaises(Exception):
                runner.parse_size(invalid)

    def test_preparation_excluded_and_batch_rate_uses_wall_clock(self):
        args = SimpleNamespace(url="https://example.test/api/perftest/site", cookie_jar="cookies.txt",
                               bytes=1048576, concurrency=2, json=True)
        head = SimpleNamespace(returncode=0, stdout='{"http_code":200}', stderr="")
        output = io.StringIO()
        with patch.object(runner.subprocess, "run", return_value=head) as head_call, \
             patch.object(runner.subprocess, "Popen", side_effect=[FakeProcess(args.bytes), FakeProcess(args.bytes)]) as get_call, \
             patch.object(runner.time, "monotonic", side_effect=[100.0, 104.0]), \
             contextlib.redirect_stdout(output):
            self.assertEqual(runner.run(args), 0)
        result = json.loads(output.getvalue())
        self.assertTrue(result["success"])
        self.assertEqual(result["batch_elapsed_seconds"], 4)
        self.assertEqual(result["aggregate_mib_per_second"], 0.5)
        self.assertEqual(result["transfers"][0]["end_to_end_mib_per_second"], 0.5)
        self.assertEqual(head_call.call_args.args[0][-1], args.url + "?bytes=1048576")
        self.assertIn("--head", head_call.call_args.args[0])
        self.assertEqual(get_call.call_count, 2)

    def test_truncated_transfer_fails_batch(self):
        args = SimpleNamespace(url="https://example.test/api/perftest/site", cookie_jar="cookies.txt",
                               bytes=10, concurrency=1, json=True)
        head = SimpleNamespace(returncode=0, stdout='{"http_code":200}', stderr="")
        output = io.StringIO()
        with patch.object(runner.subprocess, "run", return_value=head), \
             patch.object(runner.subprocess, "Popen", return_value=FakeProcess(9)), \
             patch.object(runner.time, "monotonic", side_effect=[0.0, 1.0]), \
             contextlib.redirect_stdout(output):
            self.assertEqual(runner.run(args), 1)
        self.assertFalse(json.loads(output.getvalue())["success"])

    def test_http_error_fails_batch_even_with_expected_bytes(self):
        args = SimpleNamespace(url="https://example.test/api/perftest/storage", cookie_jar="cookies.txt",
                               bytes=10, concurrency=1, json=True)
        head = SimpleNamespace(returncode=0, stdout='{"http_code":200}', stderr="")
        output = io.StringIO()
        with patch.object(runner.subprocess, "run", return_value=head), \
             patch.object(runner.subprocess, "Popen", return_value=FakeProcess(10, status=403, exit_status=22)), \
             patch.object(runner.time, "monotonic", side_effect=[0.0, 1.0]), \
             contextlib.redirect_stdout(output):
            self.assertEqual(runner.run(args), 1)
        self.assertFalse(json.loads(output.getvalue())["success"])

    def test_failed_preparation_never_starts_get(self):
        args = SimpleNamespace(url="https://example.test/api/perftest/site", cookie_jar="cookies.txt",
                               bytes=10, concurrency=1, json=True)
        head = SimpleNamespace(returncode=22, stdout='{"http_code":403}', stderr="forbidden")
        with patch.object(runner.subprocess, "run", return_value=head), \
             patch.object(runner.subprocess, "Popen") as get_call:
            with self.assertRaises(RuntimeError):
                runner.run(args)
            get_call.assert_not_called()


if __name__ == "__main__":
    unittest.main()
