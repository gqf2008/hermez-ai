{
  description = "Hermes Agent — Self-evolving AI agent system";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rustfmt" "clippy" ];
          targets = [ "wasm32-wasip1" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            cargo-component
            wasm-tools
            nodejs_22
            sqlite
            pkg-config
            openssl
            cmake
            protobuf
          ];

          shellHook = ''
            echo "Hermes Agent development environment"
            echo "  Rust:    $(rustc --version)"
            echo "  Cargo:   $(cargo --version)"
            echo "  Node:    $(node --version)"
            echo ""
            echo "Quick start:"
            echo "  cargo build --release"
            echo "  cd web && npm install && npm run dev"
          '';
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "hermes";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake
            protobuf
          ];
          buildInputs = with pkgs; [
            openssl
            sqlite
          ];
          meta = with pkgs.lib; {
            description = "Self-evolving AI agent system";
            homepage = "https://github.com/NousResearch/hermes-agent";
            license = licenses.mit;
          };
        };
      }
    );
}
