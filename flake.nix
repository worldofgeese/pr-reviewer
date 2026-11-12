{
  description = "pr-reviewer: self-hosted daemon that reviews GitHub pull requests with local AI CLI tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = ["aarch64-darwin" "aarch64-linux" "x86_64-linux"];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    packages = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in rec {
      pr-reviewer = pkgs.rustPlatform.buildRustPackage {
        pname = cargoToml.package.name;
        inherit (cargoToml.package) version;

        # Only what cargo reads, so a docs or workflow change does not rebuild.
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
          ];
        };

        cargoLock.lockFile = ./Cargo.lock;

        # nixpkgs' cargo setup hook sets LIBSQLITE3_SYS_USE_PKG_CONFIG=1, which
        # overrides rusqlite's `bundled` feature, so link nixpkgs' sqlite.
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.sqlite];

        # The suite binds loopback sockets, which the Nix sandbox refuses, and
        # it shells out to git. CI runs it instead.
        doCheck = false;

        meta = {
          description = cargoToml.package.description;
          homepage = cargoToml.package.repository;
          license = pkgs.lib.licenses.mit;
          mainProgram = "pr-reviewer";
          platforms = pkgs.lib.platforms.unix;
        };
      };
      default = pr-reviewer;
    });
  };
}
