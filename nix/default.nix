{
  stdenv,
  rustPlatform,
  lib,
  pkg-config,
  libX11,
  gtk4,
  libadwaita,
  libXtst,
  wrapGAppsHook4,
  librsvg,
  git,
  dbus,
  fontconfig,
}:
let
  # The workspace root is virtual (no [package]); the mousehop
  # binary crate lives in mousehop/ as a workspace member. Pull the
  # package name from there and the version from the workspace root,
  # which still owns [workspace.package].
  appCargoToml = fromTOML (builtins.readFile ../mousehop/Cargo.toml);
  rootCargoToml = fromTOML (builtins.readFile ../Cargo.toml);
  pname = appCargoToml.package.name;
  version = rootCargoToml.workspace.package.version;
in
rustPlatform.buildRustPackage {
  inherit pname;
  inherit version;

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    git
  ];

  buildInputs = [
    gtk4
    libadwaita
    librsvg
  ]
  ++ lib.optionals stdenv.isLinux [
    libX11
    libXtst
    dbus
    # `glyph_font` links libfontconfig directly to register the bundled
    # chord-chip faces; GTK propagates it, but declare it explicitly.
    fontconfig
  ];

  src = builtins.path {
    name = pname;
    path = lib.cleanSource ../.;
  };

  cargoLock.lockFile = ../Cargo.lock;

  # Set Environment Variables
  RUST_BACKTRACE = "full";

  postInstall = ''
    install -Dm444 mousehop/*.desktop -t $out/share/applications
    install -Dm444 mousehop-gtk/resources/*.svg -t $out/share/icons/hicolor/scalable/apps
  '';

  meta = with lib; {
    description = "Mousehop is a mouse and keyboard sharing software";
    longDescription = ''
      Mousehop is a mouse and keyboard sharing software similar to universal-control on Apple devices. It allows for using multiple pcs with a single set of mouse and keyboard. This is also known as a Software KVM switch.
      The primary target is Wayland on Linux but Windows and MacOS and Linux on Xorg have partial support as well (see below for more details).
    '';
    mainProgram = pname;
    platforms = platforms.all;
  };
}
