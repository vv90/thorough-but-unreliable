{
  description = "Development container and harness VM for thorough-but-unreliable";

  # The exact revision is pinned in flake.lock.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      imageVersion = "latest";

      harnessPackage = pkgs.rustPlatform.buildRustPackage {
        pname = "thorough-but-unreliable";
        version = "0.1.0";
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
            ./tests
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        meta.mainProgram = "harness";
      };

      # Compile an opt-in integration-test executable for use inside the VM.
      # No Podman commands execute in this derivation or on the physical host.
      podmanRuntimeTests = harnessPackage.overrideAttrs (old: {
        pname = "podman-runtime-tests";
        doCheck = false;
        nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.jq ];
        buildPhase = ''
          runHook preBuild
          cargo test --release --locked --offline --test podman_runtime \
            --no-run --message-format=json > test-build.json
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          executable=$(jq -ers '[.[] | select(.reason == "compiler-artifact" and .target.name == "podman_runtime" and .executable != null)] | if length == 1 then .[0].executable else error("expected one test executable") end' test-build.json)
          install -Dm755 "$executable" "$out/bin/podman-runtime-test"
          runHook postInstall
        '';
        meta.mainProgram = "podman-runtime-test";
      });

      # Private single-user Nix; no host store or daemon connection.
      developmentTools = with pkgs; [
        nix
        nil
        nixfmt
        statix
        deadnix
        bashInteractive
        coreutils
        findutils
        gnugrep
        gnused
        gawk
        gnutar
        gzip
        xz
        unzip
        zip
        curl
        git
        cacert
        which
        file
        ripgrep
        jq
        procps
        util-linux
        less
        ncurses
        nodejs
        shellcheck
        stdenv.cc
        rustc
        cargo
        rustfmt
        clippy
        rust-analyzer
        codex
      ];

      passwd = pkgs.writeTextDir "etc/passwd" ''
        root:x:0:0:root:/root:${pkgs.bashInteractive}/bin/bash
        dev:x:1000:1000:dev:/home/dev:${pkgs.bashInteractive}/bin/bash
      '';
      group = pkgs.writeTextDir "etc/group" ''
        root:x:0:
        dev:x:1000:
      '';
      nixConfig = pkgs.writeTextDir "etc/nix/nix.conf" ''
        experimental-features = nix-command flakes
        build-users-group =
        sandbox = false
      '';
      codexConfig = pkgs.writeText "codex-config.toml" ''
        cli_auth_credentials_store = "file"
        approval_policy = "on-request"
        sandbox_mode = "danger-full-access"
      '';
    in
    {
      nixosConfigurations.harness = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit harnessPackage; };
        modules = [ ./nixos/harness-vm.nix ];
      };

      nixosConfigurations.harness-connected-smoke = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit harnessPackage; };
        modules = [
          ./nixos/harness-vm.nix
          ./nixos/harness-connected-smoke.nix
        ];
      };

      nixosConfigurations.podman-runtime-smoke = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs.runtimeTests = podmanRuntimeTests;
        modules = [ ./nixos/podman-runtime-smoke.nix ];
      };

      packages.${system} = {
        harness = harnessPackage;
        harness-image = self.nixosConfigurations.harness.config.system.build.image;
        harness-connected-smoke-image =
          self.nixosConfigurations.harness-connected-smoke.config.system.build.image;
        podman-runtime-smoke-image =
          self.nixosConfigurations.podman-runtime-smoke.config.system.build.image;

        devImage = pkgs.dockerTools.buildLayeredImage {
          name = "localhost/thorough-but-unreliable-dev";
          tag = imageVersion;
          contents = developmentTools ++ [
            passwd
            group
            nixConfig
          ];
          # Register the included closures and keep their image GC roots.
          includeNixDB = true;
          # Store objects and directories must belong to the single-user owner.
          uid = 1000;
          gid = 1000;
          uname = "dev";
          gname = "dev";

          extraCommands = ''
            mkdir -p home/dev/.codex
            chmod 700 home/dev/.codex
            # A writable regular file, copied into a fresh named volume by Podman.
            cp ${codexConfig} home/dev/.codex/config.toml
            chmod 600 home/dev/.codex/config.toml
            mkdir -p workspaces tmp
            mkdir -p nix/store nix/var/nix/profiles/per-user/dev
            chmod u+rwx nix nix/store nix/var nix/var/nix
            chmod 1777 tmp
          '';
          fakeRootCommands = ''
            chown -R 1000:1000 home/dev workspaces nix
          '';

          config = {
            User = "1000:1000";
            WorkingDir = "/workspaces";
            Env = [
              "HOME=/home/dev"
              "USER=dev"
              "PATH=/home/dev/.nix-profile/bin:/home/dev/.local/state/nix/profile/bin:/bin"
              "SHELL=/bin/bash"
              "LANG=C.UTF-8"
              "CC=gcc"
              "NIX_REMOTE=local"
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "NIX_SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
            ];
            Labels = {
              "org.opencontainers.image.title" = "thorough-but-unreliable-dev";
              "org.opencontainers.image.version" = imageVersion;
              "org.opencontainers.image.description" =
                "Base development environment with private single-user Nix";
            };
            Cmd = [ "${pkgs.bashInteractive}/bin/bash" ];
          };
        };

      };

      checks.${system} = {
        harness-connected-smoke = harnessPackage.overrideAttrs (old: {
          pname = "harness-connected-smoke";
          requiredSystemFeatures = [ "kvm" ];
          preferLocalBuild = true;
          allowSubstitutes = false;
          nativeCheckInputs = (old.nativeCheckInputs or [ ]) ++ [
            pkgs.qemu_kvm
            pkgs.xorriso
            pkgs.netcat-openbsd
          ];
          HARNESS_SMOKE_IMAGE = "${
            self.packages.${system}.harness-connected-smoke-image
          }/harness-connected-smoke.qcow2";
          cargoTestFlags = [
            "--test"
            "connected_vm"
            "connected_vm_smoke"
          ];
          checkFlags = [
            "--ignored"
            "--exact"
            "--nocapture"
          ];
          # Run the Rust test and all its child processes inside the builder.
          # Successful builds publish the logs, ISO, and writable overlay.
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/artifacts"
            cp -a .artifacts/. "$out/artifacts/"
            runHook postInstall
          '';
        });

        podman-runtime = harnessPackage.overrideAttrs (old: {
          pname = "podman-runtime-smoke";
          requiredSystemFeatures = [ "kvm" ];
          preferLocalBuild = true;
          allowSubstitutes = false;
          nativeCheckInputs = (old.nativeCheckInputs or [ ]) ++ [ pkgs.qemu_kvm ];
          PODMAN_SMOKE_IMAGE = "${
            self.packages.${system}.podman-runtime-smoke-image
          }/podman-runtime-smoke.qcow2";
          cargoTestFlags = [
            "--test"
            "podman_vm"
            "podman_vm_smoke"
          ];
          checkFlags = [
            "--ignored"
            "--exact"
            "--nocapture"
          ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/artifacts"
            cp -a .artifacts/. "$out/artifacts/"
            runHook postInstall
          '';
        });
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = developmentTools;
      };
    };
}
