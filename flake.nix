{
  description = "Limmat: Local Immediate Automated Testing";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-utils,
      ...
    }:
    let
      # Other systems probably work too, I just don't have them to test. Feel
      # free to add them if you test them.
      supportedSystems = [ "x86_64-linux" ];
    in
    flake-utils.lib.eachSystem supportedSystems (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk = pkgs.callPackage inputs.naersk { };
      in
      rec {
        formatter = pkgs.nixfmt-tree;
        packages = rec {
          limmat = naersk.buildPackage {
            src = ./.;
          };
          default = limmat;
        };
        devShells.default = pkgs.mkShell {
          inputsFrom = [ packages.limmat] ;
          packages = [ pkgs.clippy ];
        };
      }
    );
}
