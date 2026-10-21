# Sandbox build validation

The image supplies native Pants execution without a repository bootstrap change or a checked-in sandbox profile. Run ordinary scoped `pants` commands in a writable checkout. The image's launcher recognizes the existing `default`, `python_3_10`, and `python_3_12` environment aliases, adds its baked configuration after `pants.toml`, and supplies Docker-environment fallbacks to one native environment. Python constraints still select the installed interpreter. Multiple aliases pointing directly to one local environment would cause ambiguous-environment errors.

The launcher creates an ignored `.centaur-native-pants/` directory containing the image's environment targets. It adds that directory to Git's local exclude file and explicitly includes it in Pants discovery. Tracked repository files stay unchanged. Setup is safe to repeat and refuses to replace existing user-owned files. Other Pants projects pass through unchanged. An explicit `PANTS_CONFIG_FILES`, `--pants-config-files`, or `--config-files` bypasses these defaults; `PANTS_DOCKER_EXECUTION=true` or `--docker-execution` opts into normal Docker execution.

The image includes Python interpreters, Go, protobuf tools and native build libraries. It also downloads the Go 1.25.0 toolchain during the build for runners that select that exact version, while retaining the primary Go toolchain. Native Pants runs default to no remote-cache reads or writes, avoiding developer VPN cache requirements; explicit cache settings take precedence. The launcher selects an existing proxy certificate bundle when present, otherwise the system bundle. Explicit certificate settings take precedence.

The image build runs `smoke-test.sh` through the installed launcher. Its temporary Git checkout starts with Docker execution enabled and three ordinary aliases, without a native profile, bootstrap hook or Docker fallback targets. The check exercises default, Python 3.10 and Python 3.12 tests, asserts the interpreter used, and requires exactly three passing JUnit results. Public test dependencies are fetched before the runtime package configuration is installed. Run the same check inside the built image:

```bash
bash /usr/local/share/klaviyo-sandbox/smoke-test.sh 2.33.0
```

Package access uses the existing proxy-backed configuration; no real credentials are baked into the image. Runtime acceptance additionally requires an approved package index and a scoped passing repository test with a fresh nonzero JUnit report. The fixture does not verify private package authorization, mounted overlay discovery, Docker-dependent tests or service dependencies. Docker-dependent tests need a separately provided daemon; service integration tests need their databases and other dependencies.
