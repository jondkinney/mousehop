{
  description = "Nix Flake for mousehop";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      nixpkgs,
      rust-overlay,
      self,
      ...
    }:
    let
      inherit (nixpkgs) lib;
      forEachPkgs =
        f:
        lib.genAttrs
          [
            "aarch64-darwin"
            "aarch64-linux"
            "x86_64-darwin"
            "x86_64-linux"
          ]
          (
            system:
            let
              pkgs = import nixpkgs {
                inherit system;
                overlays = [ rust-overlay.overlays.default ];
              };
              # Default toolchain for devshell
              rustToolchain = pkgs.rust-bin.stable.latest.default.override {
                extensions = [
                  # includes already:
                  # rustc
                  # cargo
                  # rust-std
                  # rust-docs
                  # rustfmt-preview
                  # clippy-preview
                  "rust-analyzer"
                  "rust-src"
                ];
              };
              # Minimal toolchain for builds (rustc + cargo + rust-std only)
              rustToolchainForBuild = pkgs.rust-bin.stable.latest.minimal;
            in
            f { inherit pkgs rustToolchain rustToolchainForBuild; }
          );
    in
    {
      packages = forEachPkgs (
        { pkgs, rustToolchainForBuild, ... }:
        let
          customRustPlatform =
            (pkgs.makeRustPlatform {
              cargo = rustToolchainForBuild;
              rustc = rustToolchainForBuild;
            }).overrideScope
              (
                _final: _previous: {
                  # The pinned nixpkgs still uses crates.io's rate-limited API
                  # endpoint. Rewrite only fixed-output crate downloads to the
                  # immutable CDN, preserving the known-good package graph for
                  # Linux and both supported macOS architectures.
                  importCargoLock = pkgs.buildPackages.callPackage "${nixpkgs}/pkgs/build-support/rust/import-cargo-lock.nix" {
                    cargo = rustToolchainForBuild;
                    fetchurl =
                      args:
                      pkgs.buildPackages.fetchurl (
                        args
                        // {
                          url = lib.replaceStrings
                            [ "https://crates.io/api/v1/crates" ]
                            [ "https://static.crates.io/crates" ]
                            args.url;
                        }
                      );
                  };
                }
              );
          mousehop = pkgs.callPackage ./nix { rustPlatform = customRustPlatform; };
        in
        {
          default = mousehop;
          inherit mousehop;
        }
      );
      devShells = forEachPkgs (
        { pkgs, rustToolchain, ... }:
        {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                rustToolchain
                pkg-config
                gtk4
                libadwaita
                librsvg
              ]
              ++ lib.optionals pkgs.stdenv.isLinux [
                libX11
                libXtst
                dbus
              ];
            env.RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };
        }
      );
      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
