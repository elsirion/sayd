{
  description = "sayd - tray-resident local text-to-speech for Wayland";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      packages.${system} = rec {
        sayd = pkgs.callPackage ./package.nix { };
        default = sayd;
      };

      # So a system flake can add `sayd` to `environment.systemPackages`
      # without threading the package through by hand.
      overlays.default = final: _prev: {
        sayd = final.callPackage ./package.nix { };
      };

      devShells.${system}.default = pkgs.mkShell {
        buildInputs = with pkgs; [
          espeak-ng
          onnxruntime
          alsa-lib
          gtk4
          libadwaita
        ];

        nativeBuildInputs = with pkgs; [
          pkg-config
          cargo
          rustc
          clippy
          rustfmt
        ];

        shellHook = ''
          # ort's load-dynamic dlopens this; a downloaded prebuilt cannot run on NixOS.
          export ORT_DYLIB_PATH=${pkgs.onnxruntime}/lib/libonnxruntime.so
          export ESPEAK_DATA_PATH=${pkgs.espeak-ng}/share/espeak-ng-data
          export ESPEAK_LIB_DIR=${pkgs.espeak-ng}/lib
          # A global CARGO_BUILD_TARGET_DIR is set on this machine and points at an
          # unrelated workspace; keep this project's artifacts local.
          export CARGO_TARGET_DIR="$PWD/target"
          export CARGO_BUILD_TARGET_DIR="$PWD/target"
          echo "sayd devshell: $(rustc --version)"
        '';
      };
    };
}
