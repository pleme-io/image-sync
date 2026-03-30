{
  description = "image-sync — smart container image cache synchronizer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    crate2nix.url = "github:nix-community/crate2nix";
    flake-utils.url = "github:numtide/flake-utils";
    substrate = {
      url = "github:pleme-io/substrate";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crate2nix, flake-utils, substrate, ... }:
    (import "${substrate}/lib/rust-service-flake.nix" {
      inherit nixpkgs crate2nix flake-utils;
    }) {
      toolName = "image-sync";
      src = self;
      repo = "pleme-io/image-sync";

      # Include crane binary in the Docker image for registry operations
      extraPackages = pkgs: [ pkgs.crane ];
    };
}
