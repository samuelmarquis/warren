{
  description = "warren — a meta-harness for Claude Code and OMP: a colony of agents from one terminal";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forEachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forEachSystem (pkgs:
        let
          inherit (pkgs) lib;
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        in
        rec {
          warren = pkgs.rustPlatform.buildRustPackage {
            pname = cargoToml.package.name;
            inherit (cargoToml.package) version;

            # Only what a build reads: a README or a note must not invalidate it.
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./tests ];
            };

            cargoLock.lockFile = ./Cargo.lock;

            # The suite spawns real daemons around ptys and unix sockets in
            # $HOME; the build sandbox gives it none of that. Run it in the
            # dev shell instead: `nix develop -c cargo test`.
            doCheck = false;

            meta = {
              inherit (cargoToml.package) description;
              homepage = cargoToml.package.repository;
              license = lib.licenses.gpl3Only;
              mainProgram = "warren";
              platforms = systems;
            };
          };

          default = warren;
        });

      apps = forEachSystem (pkgs: rec {
        warren = {
          type = "app";
          program = "${self.packages.${pkgs.system}.warren}/bin/warren";
        };
        default = warren;
      });

      devShells = forEachSystem (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });
    };
}
