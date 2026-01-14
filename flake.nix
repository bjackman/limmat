{
  description = "Limmat: Local Immediate Automated Testing";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
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
          # Limmat is hard-coded to depend on some external tools. This is fine
          # in the real world but in testing scenarios it can be handy to have a
          # Nix package that just works even in a very pure Nix environment. So
          # this adds those dependencies explicitly. PATH is suffixed so the
          # user's version of the tools take precedence.
          limmat-wrapped = pkgs.stdenv.mkDerivation {
            pname = "limmat-wrapped";
            version = limmat.version;
            src = ./.;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            installPhase = ''
              makeWrapper ${limmat}/bin/limmat $out/bin/limmat-wrapped \
                --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.git pkgs.bash ]}
            '';
          };
          default = limmat-wrapped;
        };
        devShells.default = pkgs.mkShell {
          inputsFrom = [ packages.limmat] ;
          packages = (with pkgs; [ clippy rustfmt ]);
        };
      }
    );
}
