{
  description = "Traefik D-Bus configuration provider";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      naersk,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ fenix.overlays.default ];
        };
        lib = pkgs.lib;

        buildForTarget = import ./cross-build.nix {
          inherit nixpkgs;
          inherit lib;
          inherit system;
          inherit fenix;
          inherit naersk;
        };
      in
      {
        packages = {
          x86_64-gnu = buildForTarget { target = "x86_64-unknown-linux-gnu"; }; # this will link to nix libraries and wont work in any other Linux system
          x86_64 = buildForTarget {
            target = "x86_64-unknown-linux-musl";
            binary_suffix = "x86_64";
          };
          x86_64_test = buildForTarget {
            target = "x86_64-unknown-linux-musl";
            mode = "test";
          };
          x86_64_clippy = buildForTarget {
            target = "x86_64-unknown-linux-musl";
            mode = "clippy";
          };
          aarch64 = buildForTarget {
            target = "aarch64-unknown-linux-musl";
            binary_suffix = "aarch64";
          };
          default =
            pkgs.runCommand "all-outputs" # builds outputs, statically linked
              {
                buildInputs = [
                  self.packages.${system}.x86_64_test
                  self.packages.${system}.x86_64_clippy
                ];
              }
              ''
                mkdir -p $out/bin
                ln -s ${self.packages.${system}.x86_64}/bin/systemd_traefik_configuration_provider $out/bin/systemd_traefik_configuration_provider_x86_64
                ln -s ${
                  self.packages.${system}.aarch64
                }/bin/systemd_traefik_configuration_provider $out/bin/systemd_traefik_configuration_provider_aarch64
              '';
        };
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            (pkgs.fenix.complete.withComponents [
              "cargo"
              "clippy"
              "rust-src"
              "rustc"
              "rustfmt"
              "rust-analyzer"
            ])
            cargo-nextest
            yq-go
            gnumake
            zbus-xmlgen
          ];
        };
        formatter = pkgs.nixpkgs-fmt;
      }
    );
}
