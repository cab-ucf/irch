{
  description = "irch: private, peer-to-peer community chat over iroh";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  outputs = { self, nixpkgs }:
    let
      each = f: nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ]
        (s: f nixpkgs.legacyPackages.${s});
    in {
      packages = each (pkgs: rec {
        irch = pkgs.callPackage ./package.nix { };
        default = irch;
      });
      nixosModules.default = ./nixos.nix;
    };
}
