{
  description = "NerfESP esp32 (xtensa) rust flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
    flake-parts.url = "github:hercules-ci/flake-parts";
    fenix.url = "github:nix-community/fenix/monthly";
    esp-rs-nix = {
      url = "github:leighleighleigh/esp-rs-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } ({ ... }: {
      systems = import inputs.systems;
      perSystem = { pkgs, system, inputs', ... }:
      let
        fpkgs = inputs'.fenix.packages;
        # esp toolchain: cargo, rustc (+ xtensa core/alloc), rust-src, xtensa gcc / gdb
        esp-rs = inputs'.esp-rs-nix.packages.esp-rs;
      in {
        devShells.default =
        pkgs.mkShell {
          buildInputs = with pkgs; [
            esp-rs
            fpkgs.complete.rustfmt
            fpkgs.rust-analyzer

            # extra cargo tools
            cargo-edit
            cargo-expand
            cargo-bloat

            # for flashing
            espflash
          ];

          # set the rust src for rust_analyzer
          RUST_SRC_PATH = "${esp-rs}/lib/rustlib/src/rust/library";
        };
      };
    });
}
