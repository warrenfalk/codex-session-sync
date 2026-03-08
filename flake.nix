{
  description = "Codex session sync service";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.rust-bin.stable.latest.minimal.override {
          extensions = [ "clippy" "rust-src" "rustfmt" ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        package = rustPlatform.buildRustPackage {
          pname = "codex-session-sync";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ];
          nativeCheckInputs = [ pkgs.git ];
          buildInputs = [ pkgs.sqlite ];
        };
      in
      {
        packages.default = package;

        apps.default = flake-utils.lib.mkApp {
          drv = package;
        };

        checks.default = package;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            cargo-nextest
            git
            pkg-config
            sqlite
          ];

          shellHook = ''
            export CARGO_HOME="$PWD/.cargo-home"
            export RUSTUP_HOME="$PWD/.rustup-home"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
