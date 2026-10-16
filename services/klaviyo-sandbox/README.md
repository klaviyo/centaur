# Klaviyo sandbox

Build the base agent image and this extension in the same Docker daemon:

```bash
docker build --target sandbox -f services/sandbox/Dockerfile -t klaviyo-centaur-agent:local .
docker build -f services/klaviyo-sandbox/Dockerfile -t klaviyo-centaur-sandbox:local .
```

The image includes a pip configuration containing only the placeholders
`KREPO_ARTIFACTORY_USERNAME` and `KREPO_ARTIFACTORY_PASSWORD`. k-repo's Pants
bootstrap reads the same file and derives Go's registry configuration from it.
Centaur supplies the proxy address and CA certificate at runtime. Pants uses
that CA; pip uses Centaur's `REQUESTS_CA_BUNDLE`, and uv uses native TLS trust.

## Configure package access

Run the provisioning script from the repository root on an operator machine
with an authenticated JFrog index in `~/.pip/pip.conf`. Supply an iron-control
operator API key through `IRON_CONTROL_API_KEY`, and select an existing role:

```bash
python3 services/klaviyo-sandbox/configure_artifactory.py \
  --console-url https://<console-host> \
  --role <package-access-role>
```

Use `infra` to give package access to principals holding Centaur's shared infra
role, or select a narrower role already assigned to the intended principals.
The script does not assign roles to principals or change deployment images.

The script reads and URL-decodes the username and password, stores them as two
encrypted, write-only control-plane sources, and grants their replacements to
the selected role. Re-running it rotates those same two credentials without
duplicating grants. The real values are never printed or copied into the image.
They are delivered to authorized iron-proxy instances through proxy sync.

Replacement applies only to `Authorization` on GET/HEAD requests to the JFrog
PyPI index, PyPI artifacts, and Go virtual registry paths listed in the script.
It grants no package-upload or Artifactory administration endpoints. These
request restrictions do not change the underlying credential's own permissions.

After publication, the deployment must select the new sandbox image before its
pip configuration is available to newly created sandboxes.

## Validation

```bash
python3 -m unittest discover -s services/klaviyo-sandbox -p 'test_*.py' -v
```

Validate a private package download from a sandbox after provisioning its
principal. Check that its pip config still contains only placeholders.
