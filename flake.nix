{
  description = "recto — a review-first terminal diff viewer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        # nixpkgs `-unstable-<date>` convention, date pulled from flake
        # metadata (YYYYMMDD…) so the version tracks the pinned rev. Falls
        # back to a placeholder for a dirty tree with no lastModifiedDate.
        lastMod = self.lastModifiedDate or "19700101000000";
        date = "${builtins.substring 0 4 lastMod}-${builtins.substring 4 2 lastMod}-${builtins.substring 6 2 lastMod}";
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "recto";
          version = "0-unstable-${date}";

          src = builtins.path {
            path = ./.;
            name = "recto-source";
          };

          # Read Cargo.lock directly — no cargoHash to maintain, and the source
          # is the flake input itself, so shipping never touches a hash again.
          cargoLock.lockFile = ./Cargo.lock;

          # Repository integration tests exercise both supported backends.
          nativeCheckInputs = with pkgs; [
            git
            jujutsu
          ];

          meta = {
            description = "jj-first terminal diff viewer for reviewing agent-authored changes";
            homepage = "https://github.com/phinze/recto";
            license = pkgs.lib.licenses.mit;
            maintainers = [ ];
            mainProgram = "recto";
          };
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            pkg-config
          ];

          env = {
            RUST_BACKTRACE = "1";
          };
        };
      }
    );
}
