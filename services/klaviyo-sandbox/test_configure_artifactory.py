"""Tests for registry parsing, scoped grants, and placeholder image configuration."""

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from configure_artifactory import Console, RULES, configure, read_credentials


class ProvisioningTests(unittest.TestCase):
    def test_parse_encoded_credentials_without_interpolation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pip.conf"
            path.write_text("[global]\nextra-index-url = https://user%40example:pass%25word@klaviyo.jfrog.io/artifactory/api/pypi/pypi/simple\n")
            self.assertEqual(read_credentials(path), ("user@example", "pass%word"))

    def test_reject_unrelated_host(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pip.conf"
            path.write_text("[global]\nindex-url = https://user:password@example.com/simple\n")
            with self.assertRaises(ValueError):
                read_credentials(path)

    def test_grants_are_scoped_and_not_duplicated(self):
        class FakeConsole:
            def __init__(self):
                self.calls = []

            def grants(self, role):
                return [{"static_secret_id": "ssr_username"}]

            def request(self, method, path, data=None):
                self.calls.append((method, path, data))
                if method == "GET":
                    return {"data": {"id": "role_test"}}
                if method == "PUT":
                    return {"data": {"id": "ssr_" + path.rsplit("-", 1)[-1]}}
                return {"data": {"id": "grant_test"}}

        console = FakeConsole()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            configure(console, ("synthetic-user", "synthetic-password"), "test-role")
        secrets = [data for method, _, data in console.calls if method == "PUT"]
        self.assertEqual(len(secrets), 2)
        for secret in secrets:
            self.assertEqual(secret["rules"], RULES)
            self.assertEqual(secret["replace_config"]["match_headers"], ["Authorization"])
            self.assertEqual(secret["source"]["source_type"], "control_plane")
            self.assertEqual(secret["rules"][0]["http_methods"], ["GET", "HEAD"])
        grants = [data for method, _, data in console.calls if method == "POST"]
        self.assertEqual(grants, [{"role_id": "role_test", "static_secret_id": "ssr_password"}])
        self.assertNotIn("synthetic-user", output.getvalue())
        self.assertNotIn("synthetic-password", output.getvalue())

    def test_operator_api_requires_secure_origin(self):
        with self.assertRaises(ValueError):
            Console("http://example.com", "synthetic-key")
        Console("http://127.0.0.1:3000", "synthetic-key")


if __name__ == "__main__":
    unittest.main()
