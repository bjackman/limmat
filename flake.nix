{
  description = "Limmat: Local Immediate Automated Testing";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-25.05";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      ...
    }:
    {

      packages =
        let
          # Other systems probably work too, I just don't have them to test. Feel
          # free to add them if you test them.
          supportedSystems = [ "x86_64-linux" ];
          forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
        in
        forAllSystems (
          system:
          let
            pkgs = import nixpkgs { inherit system; };
            naersk = pkgs.callPackage inputs.naersk { };
          in
          {
            default = naersk.buildPackage {
              src = ./.;
            };
          }
        );
    };
}
