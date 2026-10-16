"""Provision host-scoped Basic Auth replacements from an operator's pip config."""

import argparse
import configparser
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

HOST = "klaviyo.jfrog.io"
RULES = [{
    "host": HOST,
    "http_methods": ["GET", "HEAD"],
    "paths": [
        "/artifactory/api/pypi/pypi/*",
        "/artifactory/pypi/*",
        "/artifactory/api/go/go-virtual/*",
    ],
}]


def read_credentials(path):
    config = configparser.ConfigParser(interpolation=None)
    with path.open() as source:
        config.read_file(source)
    credentials = set()
    for section in config.sections():
        for option in ("index-url", "extra-index-url"):
            for value in config.get(section, option, fallback="").split():
                url = urllib.parse.urlsplit(value)
                if url.scheme == "https" and url.hostname == HOST:
                    if url.username and url.password:
                        credentials.add((urllib.parse.unquote(url.username),
                                         urllib.parse.unquote(url.password)))
    if len(credentials) != 1:
        raise ValueError("Expected one credential pair for JFrog in pip.conf")
    return credentials.pop()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Console:
    def __init__(self, url, key):
        parsed = urllib.parse.urlsplit(url)
        local = parsed.hostname in ("127.0.0.1", "localhost", "::1")
        if parsed.scheme != "https" and not (local and parsed.scheme == "http"):
            raise ValueError("Console URL must use HTTPS or loopback HTTP")
        if parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError("Console URL must not contain credentials, query or fragment")
        self.url = url.rstrip("/")
        self.key = key
        self.opener = urllib.request.build_opener(NoRedirect)

    def request(self, method, path, data=None):
        request = urllib.request.Request(
            self.url + "/api/v1" + path,
            method=method,
            headers={"Authorization": "Bearer " + self.key,
                     "Content-Type": "application/json"},
            data=json.dumps({"data": data}).encode() if data is not None else None,
        )
        try:
            with self.opener.open(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"Console {method} request failed: HTTP {error.code}") from None
        except urllib.error.URLError:
            raise RuntimeError("Console request failed; connection details withheld") from None

    def grants(self, role_id):
        page = 1
        grants = []
        while True:
            response = self.request("GET", f"/roles/{role_id}/grants?page={page}")
            grants.extend(response["data"])
            if page >= response.get("meta", {}).get("total_pages", 1):
                return grants
            page += 1


def configure(console, credentials, role_name):
    role = console.request("GET", "/roles/lookup/" + urllib.parse.quote(role_name, safe=""))["data"]
    granted = {grant.get("static_secret_id") for grant in console.grants(role["id"])}
    for field, value in zip(("USERNAME", "PASSWORD"), credentials):
        foreign_id = "krepo-artifactory-" + field.lower()
        secret = console.request("PUT", "/static_secrets/" + foreign_id, {
            "foreign_id": foreign_id,
            "name": "k-repo Artifactory " + field.lower(),
            "description": "Package-download Basic Auth replacement",
            "replace_config": {
                "proxy_value": "KREPO_ARTIFACTORY_" + field,
                "match_headers": ["Authorization"],
            },
            "source": {"source_type": "control_plane", "secret": value, "config": {}},
            "rules": RULES,
        })["data"]
        if secret["id"] not in granted:
            console.request("POST", "/grants", {
                "role_id": role["id"], "static_secret_id": secret["id"],
            })
    print("Artifactory replacements configured and granted; credential values withheld.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--console-url", required=True)
    parser.add_argument("--role", required=True, help="Existing role to receive package access")
    parser.add_argument("--pip-conf", type=Path, default=Path.home() / ".pip/pip.conf")
    args = parser.parse_args()
    key = os.environ.get("IRON_CONTROL_API_KEY", "").strip()
    if not key:
        parser.error("Set IRON_CONTROL_API_KEY in the operator environment")
    try:
        configure(Console(args.console_url, key), read_credentials(args.pip_conf), args.role)
    except (ValueError, RuntimeError, OSError, configparser.Error):
        print("Provisioning failed; credentials and response bodies withheld.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
