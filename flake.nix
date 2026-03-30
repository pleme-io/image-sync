{
  description = "image-sync — smart container image cache synchronizer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crate2nix = {
      url = "github:nix-community/crate2nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    substrate = {
      url = "github:pleme-io/substrate";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.fenix.follows = "fenix";
    };
    forge = {
      url = "github:pleme-io/forge";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.fenix.follows = "fenix";
      inputs.substrate.follows = "substrate";
      inputs.crate2nix.follows = "crate2nix";
    };
  };

  outputs = { self, nixpkgs, crate2nix, flake-utils, substrate, forge, ... }:
    (import "${substrate}/lib/build/rust/tool-image-flake.nix" {
      inherit nixpkgs crate2nix flake-utils forge;
    }) {
      toolName = "image-sync";
      src = self;
      repo = "pleme-io/image-sync";
      tag = "0.1.0";
      extraContents = pkgs: [ pkgs.crane ];
      architectures = ["amd64" "arm64"];
    };
}
