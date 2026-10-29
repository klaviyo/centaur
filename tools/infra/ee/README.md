# EE remote workspace

Supply the full WSS SSH URL on each operation:

```bash
ee exec --url wss://chat.dev.example.com/events/events -- python -m pytest path/to/test.py
ee read --url wss://chat.dev.example.com/events/events path/to/file
ee write --url wss://other.dev.example.com/events/events path/to/file < local-file
```

Commands operate inside the configured remote workspace. For shell syntax, use
`ee exec --url URL -- sh -lc 'command'`. One saved personal key can authenticate
to multiple destinations; the deployment controls allowed hosts, paths, and SSH
usernames. WSS gateways are authenticated by their verified TLS identity.

The control plane supplies `CENTAUR_EE_SSH`. It contains a disposable client
key, the paired proxy's pinned host key, and one destination alias. It never
contains an upstream EE private key. The tool uses non-interactive OpenSSH
and does not retry commands after connection failures. Remote descendants can
outlive a disconnected SSH channel; inspect the remote result before retrying.

Operator setup is documented in `docs/pages/operate/ee-ssh.mdx`.
