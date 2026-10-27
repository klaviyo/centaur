# EE remote workspace

`ee exec -- python -m pytest path/to/test.py`, `ee read path/to/file`, and
`ee write path/to/file < local-file` operate inside the configured remote
workspace. For shell syntax, use `ee exec -- sh -lc 'command'`.

The control plane supplies `CENTAUR_EE_SSH`. It contains a disposable client
key, the paired proxy's pinned host key, and one destination alias. It never
contains an upstream EE private key. The tool uses non-interactive OpenSSH
and does not retry commands after connection failures. Remote descendants can
outlive a disconnected SSH channel; inspect the remote result before retrying.

Operator setup is documented in `docs/pages/operate/ee-ssh.mdx`.
