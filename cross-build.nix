{
  nixpkgs,
  lib,
  system,
  fenix,
  naersk,
  ...
}:
{
  target,
  mode ? "build",
  binary_suffix ? "",
}:
let
  crossPkgs = (
    (import nixpkgs) {
      inherit system;
      crossSystem = {
        config = target;
      };
    }
  );

  toolchain =
    with fenix.packages.${crossPkgs.stdenv.buildPlatform.system};
    combine [
      default.rustc
      default.cargo
      default.clippy
      targets.${crossPkgs.stdenv.targetPlatform.config}.latest.rust-std
    ];

  naersk' = naersk.lib.${system}.override {
    cargo = toolchain;
    rustc = toolchain;
  };

  rustTargetPlatform = crossPkgs.stdenv.targetPlatform.rust.rustcTarget;
  rustTargetPlatformUpper = lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] rustTargetPlatform);
  targetCc = "${crossPkgs.stdenv.cc}/bin/${crossPkgs.stdenv.cc.targetPrefix}cc";
in
naersk'.buildPackage {
  src = ./.;
  strictDeps = true;
  CARGO_BUILD_TARGET = "${crossPkgs.stdenv.targetPlatform.config}";
  "CC_${rustTargetPlatform}" = "${targetCc}";
  "CARGO_TARGET_${rustTargetPlatformUpper}_LINKER" = "${targetCc}";
  depsBuildBuild = [ crossPkgs.stdenv.cc ];
  inherit mode;
  postInstall =
    if binary_suffix != "" then
      ''
        ln -s $out/bin/systemd_traefik_configuration_provider $out/bin/systemd_traefik_configuration_provider_${binary_suffix}
      ''
    else
      "";
}
