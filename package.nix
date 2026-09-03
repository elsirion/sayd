{ lib
, rustPlatform
, pkg-config
, wrapGAppsHook4
, alsa-lib
, espeak-ng
, glib
, gtk4
, libadwaita
, onnxruntime
}:

rustPlatform.buildRustPackage {
  pname = "sayd";
  version = "0.2.0";

  # Named explicitly rather than filtered by hand or by `cleanSource`. The
  # repo carries a `models` symlink pointing at a ~570 MB directory of ONNX
  # weights and voice packs outside the tree; `cleanSource` keeps symlinks,
  # so the store path would either swallow the weights or carry a dangling
  # link. Listing what the build actually reads makes that unspellable, and
  # keeps `target/` out of the hash while it is at it.
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./crates
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    # GTK4/libadwaita: the settings window needs the GSettings schemas and
    # the icon theme found at run time, not just linked at build time.
    wrapGAppsHook4
  ];

  buildInputs = [
    alsa-lib
    espeak-ng
    glib
    gtk4
    libadwaita
  ];

  # `sayd-g2p/build.rs` reads this to find `libespeak-ng`, rather than
  # letting a -sys crate compile espeak from source.
  ESPEAK_LIB_DIR = "${espeak-ng}/lib";

  # The desktop file exists so hosts can resolve the settings window to an
  # icon: on Wayland that lookup goes app_id -> `sayd.desktop` -> `Icon=`,
  # and GTK derives the app_id from the program name. The tray does not use
  # this -- it sends its state icons over the bus as SNI pixmaps.
  postInstall = ''
    install -Dm444 crates/sayd/assets/sayd.svg \
      $out/share/icons/hicolor/scalable/apps/sayd.svg
    mkdir -p $out/share/applications
    cat > $out/share/applications/sayd.desktop <<DESKTOP
    [Desktop Entry]
    Type=Application
    Name=sayd
    GenericName=Text to speech
    Comment=Reads notifications, the selection and the clipboard aloud
    Exec=sayd
    Icon=sayd
    Terminal=false
    Categories=Utility;Accessibility;
    Keywords=tts;speech;speak;
    DESKTOP
  '';

  # `ort` is built with `load-dynamic` and no default features, so ONNX
  # Runtime is never linked -- it is `dlopen`ed at run time from
  # `ORT_DYLIB_PATH`, or from the loader path if that is unset. On NixOS it
  # is on neither: measured, `libonnxruntime.so` is absent from the ld cache
  # and from `/run/current-system/sw/lib`, so an unwrapped binary starts and
  # then dies on the first synthesis with "could not load ONNX Runtime".
  #
  # `--set-default`, not `--set`: a user pointing at their own build of
  # either library should win over this.
  preFixup = ''
    gappsWrapperArgs+=(
      --set-default ORT_DYLIB_PATH "${onnxruntime}/lib/libonnxruntime.so"
      --set-default ESPEAK_DATA_PATH "${espeak-ng}/share/espeak-ng-data"
    )
  '';

  # The `sayd` binary's own suite needs a Wayland compositor (the settings
  # window calls `present()`) and a session bus (the D-Bus interface tests),
  # neither of which exists in the sandbox. `scripts/headless.sh` is how
  # those are run -- see its doc comment for why a nested compositor with an
  # XDG_RUNTIME_DIR of its own is the only safe way. The crates that need
  # neither are checked here rather than skipping the phase wholesale.
  cargoTestFlags = [
    "-p"
    "sayd-core"
    "-p"
    "sayd-g2p"
    "-p"
    "sayd-kokoro"
    "-p"
    "sayd-misaki-en"
  ];

  meta = {
    description = "Local text-to-speech daemon for Wayland: speaks the selection on a keybind";
    homepage = "https://github.com/elsirion/sayd";
    license = lib.licenses.mit;
    mainProgram = "sayd";
    platforms = lib.platforms.linux;
  };
}
