#!/usr/bin/env bash
set -euo pipefail
exec uv run --no-project --no-python-downloads --python /opt/krepo/python/3.12/bin/python3.12 \
  python /usr/local/share/klaviyo-sandbox/pants_native.py "$@"
