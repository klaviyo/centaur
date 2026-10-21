{
  description = "Development environment for k-repo responsible for installing native dependencies, toolchains, and automated virtual environment setup";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    nixpkgs-latest.url = "github:NixOS/nixpkgs/nixos-25.11";
    nixpkgs-uv.url = "github:NixOS/nixpkgs/nixos-26.05";
    # Last nixpkgs commit with nodejs_24 at exactly 24.14.0 (binary-cached for
    # all four default systems). Must match [nodejs].version in pants.toml,
    # which Pants requires exactly, and docker/ci/pants/Dockerfile.
    nixpkgs-node.url = "github:NixOS/nixpkgs/fdc7b8f7b30fdbedec91b71ed82f36e1637483ed";
    utils.url = "github:numtide/flake-utils";
  };
  outputs =
    { nixpkgs, nixpkgs-latest, nixpkgs-uv, nixpkgs-node, utils, ... }:
    utils.lib.eachDefaultSystem (
      system:
      let
        pythonVersion = "3.10.18";
        pluginsPythonVersion = "3.14.3";
        upgradePythonVersion = "3.12.9";
        pyflinkPythonVersion = "3.11.12";

        # Import latest nixpkgs to override uv
        pkgs-uv = import nixpkgs-uv { inherit system; };
        pkgs-node = import nixpkgs-node { inherit system; };
        # Import nixpkgs with system-specific configuration and allow unfree packages
        pkgs-latest = import nixpkgs-latest { inherit system; };
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [
            (final: prev: { go_1_25 = pkgs-latest.go_1_25; })
            (final: prev: {
              # protobuf 33.1 prebuilt from the 25.11 binary cache. Overriding
              # src on the 25.05 package (the previous approach) forced a
              # ~15 minute source compile on every machine whenever the dev
              # shell rebuilt.
              protobuf = pkgs-latest.protobuf_33;
            })
            (final: prev: { uv = pkgs-uv.uv; })
            (final: prev: { nodejs_24 = pkgs-node.nodejs_24; })
          ];
        };

        # System libraries and runtime dependencies for the application, including compression,
        # database clients, cryptography, image processing, and XML parsing libraries
        appLibs = with pkgs; [
          brotli
          bzip2
          freetype
          pbzip2
          libffi
          libmemcached
          libtiff
          libxml2
          lz4
          mysql80
          openssl_3
          readline
          snappy
          sqlite
          xmlsec
          zlib
          zstd
        ];

        # Build configuration for native dependencies:
        # - nixIncludeFlags: C/C++ header search paths (-I flags) for appLibs
        # - nixLibFlags: Library search paths (-L flags) for linking appLibs
        # - sdkPath: macOS SDK path from apple-sdk_15 package
        nixIncludeFlags = builtins.concatStringsSep " " (map (lib: "-I${lib.dev or lib}/include") appLibs);
        # Filter library paths to only include those that actually exist to avoid linker warnings
        nixLibFlags = builtins.concatStringsSep " " (builtins.filter (x: x != null) (map (lib:
          let candidate = "${lib.lib or lib.out or lib}/lib";
          in if builtins.pathExists candidate then "-L${candidate}" else null
        ) appLibs));

        # Darwin-only toolchain pieces. On Linux these packages either don't
        # exist (apple-sdk_15, darwin.libresolv) or would break the gcc-based
        # stdenv (libc++ headers), so they only evaluate behind isDarwin and
        # the corresponding shell flags collapse to empty strings.
        isDarwin = pkgs.stdenv.isDarwin;
        optionalString = pkgs.lib.optionalString;
        libcxxIncludeFlag = optionalString isDarwin "-I${pkgs.llvmPackages.libcxx.dev}/include/c++/v1 ";
        appleSdkSysrootFlag = optionalString isDarwin " -isysroot ${pkgs.apple-sdk_15}/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk";
        libresolvLibFlag = optionalString isDarwin " -L${pkgs.darwin.libresolv}/lib";
      in
      {
        devShells.default = pkgs.mkShell {
          env = {
            IS_NIX_INITIALIZED              = "true";
          };

          # Development environment dependencies including toolchains, build tools, linters,
          # formatters, utilities, and platform SDKs, combined with application libraries
          buildInputs =
            with pkgs;
            [
              _1password-cli
              buf
              coreutils
              docker-credential-helpers
              amazon-ecr-credential-helper
              git
              google-cloud-sdk
              gnumake
              go_1_25
              gotools
              hadolint
              jq
              nodejs_24
              nixfmt-rfc-style
              pkg-config
              protobuf # 33.1 (v6.33.1)
              protoc-go-inject-tag # 1.4.0
              ripgrep
              shellcheck
              uv
              wget
              xz
              yq
            ]
            ++ pkgs.lib.optionals isDarwin [
              pkgs.apple-sdk_15
              pkgs.darwin.libresolv
              pkgs.llvmPackages.libcxx
            ]
            ++ appLibs;

          # Shell initialization script that configures the Python environment and build flags.
          # Cleans pyenv from PATH, sets up uv-managed Python virtual environment, configures
          # C/C++ compiler flags for native dependencies, and activates the virtual environment.
          shellHook = ''
            # Trap errors, send to Sentry if error occurred
            trap 'NIX_FLAKE_SHELLHOOK_FAILED=1' ERR

            export PATH=$(echo "$PATH" | tr ':' '\n' | grep -v -e '/pyenv-virtualenv.*shims' -e '/.pyenv/shims' | tr '\n' ':' | sed 's/:*$//')
            unset PYTHONPATH

            # Check if .python-version exists and is not a semver format
            if [ -f .python-version ]; then
              PYTHON_VERSION_CONTENT=$(cat .python-version | tr -d '\n' | tr -d ' ')
              # Check if content matches semver pattern (X.Y.Z or X.Y.Z.W)
              if ! echo "$PYTHON_VERSION_CONTENT" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?$'; then
                echo "Found non-semver .python-version file, backing up to .python-version.bak"
                mv .python-version .python-version.bak
                echo "${pythonVersion}" > .python-version
              fi
            else
              # Create .python-version if it doesn't exist
              echo "${pythonVersion}" > .python-version
            fi

            export UV_PYTHON_INSTALL_DIR=$PWD/.direnv/share/uv/python
            export UV_PYTHON=${pythonVersion}
            export UV_CACHE_DIR=$PWD/.direnv/uv

            export CFLAGS="${libcxxIncludeFlag}${nixIncludeFlags}"
            export CPPFLAGS="${libcxxIncludeFlag}${nixIncludeFlags}"
            export LDFLAGS="${nixLibFlags}"
            export PKG_CONFIG_PATH="${pkgs.mysql80.out}/lib/pkgconfig:$PKG_CONFIG_PATH"

            # CGO-specific environment variables for Go builds with C dependencies
            # Include SDKROOT for framework search paths (darwin only)
            export CGO_CFLAGS="$CFLAGS${appleSdkSysrootFlag}"
            export CGO_CPPFLAGS="$CPPFLAGS"
            export CGO_LDFLAGS="$LDFLAGS${libresolvLibFlag}${appleSdkSysrootFlag}"

            if ! rg -q "version_info = ${upgradePythonVersion}" .upgrade-venv/pyvenv.cfg 2>/dev/null; then
                echo "Setting up python venv for upgrade"
                rm -rf .upgrade-venv
                ${pkgs.uv}/bin/uv venv --python ${upgradePythonVersion} --seed .upgrade-venv
            fi

            if ! rg -q "version_info = ${pyflinkPythonVersion}" .pyflink-venv/pyvenv.cfg 2>/dev/null; then
                echo "Setting up python venv for pyflink"
                rm -rf .pyflink-venv
                ${pkgs.uv}/bin/uv venv --python ${pyflinkPythonVersion} --seed .pyflink-venv
            fi

            if ! rg -q "version_info = ${pluginsPythonVersion}" .plugins-venv/pyvenv.cfg 2>/dev/null; then
                echo "Setting up python venv for plugins"
                rm -rf .plugins-venv
                ${pkgs.uv}/bin/uv venv --python ${pluginsPythonVersion} --seed .plugins-venv
            fi

            if ! rg -q "version_info = ${pythonVersion}" .venv/pyvenv.cfg 2>/dev/null; then
                echo "Setting up python venv"
                rm -rf .venv
                ${pkgs.uv}/bin/uv venv --seed .venv
            fi
            source .venv/bin/activate

            export MAINLINE_PYTHON=$PWD/.venv/bin/python

            export PANTS_PYTHON_BOOTSTRAP_SEARCH_PATH="['$PWD/.venv/bin', '$PWD/.plugins-venv/bin', '$PWD/.upgrade-venv/bin', '$PWD/.pyflink-venv/bin']"

            # Increase timeout for case where uv packages take too long to download
            export UV_HTTP_TIMEOUT=60

            # Ensure go-installed binaries are in PATH (avoid duplicates)
            GOPATH_BIN="$(go env GOPATH)/bin"
            if [[ ":$PATH:" != *":$GOPATH_BIN:"* ]]; then
              export PATH="$GOPATH_BIN:$PATH"
            fi

            # Install correct protoc plugin versions
            [[ $(protoc-gen-go --version 2> /dev/null) == 'protoc-gen-go v1.36.11' ]] || go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.36.11
            [[ $(protoc-gen-go-grpc --version 2> /dev/null) == 'protoc-gen-go-grpc 1.6.1' ]] || go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.6.1

            # Unset the DEVELOPER_DIR_FOR_TARGET causing issues between conflicting Apple SDK versions
            unset DEVELOPER_DIR_FOR_TARGET
          '';
        };
      }
    );
}
