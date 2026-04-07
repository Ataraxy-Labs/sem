{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  buildInputs = with pkgs.buildPackages; [
    openssl
    pkg-config

    (rust-bin.stable.latest.default)
    rust-analyzer
  ];
}
