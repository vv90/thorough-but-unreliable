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
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        meta.mainProgram = "harness";
      };

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

      packages.${system} = {
        harness = harnessPackage;
        harness-image = self.nixosConfigurations.harness.config.system.build.image;

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

      devShells.${system}.default = pkgs.mkShell {
        packages = developmentTools;
      };
    };
}
