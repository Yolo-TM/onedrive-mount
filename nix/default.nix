# Stub — the canonical package is defined in ../flake.nix.
# This file previously copied pre-built binaries and is no longer used.
# All builds go through rustPlatform.buildRustPackage in the flake.
{ pkgs ? import <nixpkgs> {} }: builtins.throw
  "Use the flake: nix build github:youruser/onedrive-mount or add to flake inputs."
