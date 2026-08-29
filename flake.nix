{
  description = "ntfs-reader development shell with native Rust and Windows MSVC cross-build tooling";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.rust-bin.stable."1.94.1".default.override {
          extensions = [ "clippy" "rustfmt" ];
          targets = [
            "x86_64-pc-windows-msvc"
            "i686-pc-windows-msvc"
          ];
        };
      in {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            cargo-xwin
            clang
            llvm
            lld
          ];

          shellHook = ''
            echo "ntfs-reader dev shell"
            echo "  lint:       cargo fmt --check && cargo clippy"
            echo "  MSVC build: cargo xwin build --target x86_64-pc-windows-msvc"
            echo "  MSVC tests: cargo xwin test --no-run --target x86_64-pc-windows-msvc"
          '';
        };
      });
}
