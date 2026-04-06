{
  pkgs ? import <nixpkgs> {},
  lib,
  ...
}:
pkgs.rustPlatform.buildRustPackage rec {
  pname = "sem";
  version = let
    crate_name = pname + "-cli";
  in
    (builtins.fromTOML (lib.readFile "${src}/${crate_name}/Cargo.toml")).package.version;

  src = ./crates;
  cargoLock = {
    lockFile = "${src}/Cargo.lock";
    outputHashes = {
      # "dummy-0.14.0" = lib.fakeHash;
    };
  };

  # disable tests
  checkType = "debug";
  doCheck = false;

  nativeBuildInputs = with pkgs; [
    installShellFiles
    pkg-config

    llvmPackages.clang
    clang
  ];
  buildInputs = with pkgs; [
    openssl
    pkg-config

    (rust-bin.stable.latest.default)
  ];

  # postInstall = ''
  #   installShellCompletion --cmd ${pname} \
  #     --bash ./autocompletion/${pname}.bash \
  #     --fish ./autocompletion/${pname}.fish \
  #     --zsh  ./autocompletion/_${pname}
  # '';
}
