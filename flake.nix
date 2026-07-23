{
  description = "mikanani-cli";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
          ];
        };

        # Build inputs for the development environment
        buildInputs = with pkgs; [
          rustToolchain
          cargo-audit
          rust-analyzer
        ];

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];
      in
      {
        devShell = pkgs.mkShell {
          inherit buildInputs nativeBuildInputs;

          shellHooks = ''
            echo "Workspace initialized. Happy coding!"
            echo "Rust version: $(rustc --version)"
            echo "Cargo version: $(cargo --version)"
          '';
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "mikan";
          version = "0.3.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          inherit nativeBuildInputs;

          meta = with pkgs.lib; {
            description = "Interactive downloader for Mikan Project RSS feeds";
            license = licenses.mit;
            mainProgram = "mikan";
          };
        };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/mikan";
        };
      }
    );
}

