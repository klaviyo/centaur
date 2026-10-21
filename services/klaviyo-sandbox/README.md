# Sandbox build validation

The image sets `PANTS_DOCKER_EXECUTION=false` rather than overriding Pants environment names. Repositories must provide a native execution profile and select it during bootstrap. Keep a single compatible local environment and use Docker-environment fallbacks to preserve aliases. Multiple names pointing directly to one local target cause an ambiguous-environment error.

The image build runs `smoke-test.sh` through the installed Pants launcher. The isolated workspace exercises the default, Python 3.10, and Python 3.12 environment aliases without a Docker daemon, asserts the interpreter version in each test, and requires three passing JUnit results. It uses public test dependencies before the runtime package configuration is installed. No private repository or credentials are needed to build the image.

Run the same check inside the built image:

```bash
bash /usr/local/share/klaviyo-sandbox/smoke-test.sh 2.33.0
```

This checks the image's native Pants execution path. Acceptance for a repository additionally requires a writable checkout with its native profile and bootstrap installed, an approved package-index configuration, and a scoped passing repository test with a fresh nonzero JUnit report. The build fixture does not verify private package authorization, mounted overlay discovery, Docker-dependent tests, or service dependencies.

The runtime certificate bundle must exist at `PANTS_CA_CERTS_PATH`. The image build uses the system bundle; the running sandbox uses its proxy CA. Package access goes through the existing proxy-backed configuration. Do not put real credentials in the image, replace a private index to bypass authorization, or treat an absent dependency as a passing check.
