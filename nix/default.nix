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
}:
let
  cargoToml = fromTOML (builtins.readFile ../Cargo.toml);
  pname = cargoToml.package.name;
  # The root package inherits version via `version.workspace = true`,
  # so the real version lives under [workspace.package].
  version = cargoToml.workspace.package.version;
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
  ];

  src = builtins.path {
    name = pname;
    path = lib.cleanSource ../.;
  };

  cargoLock.lockFile = ../Cargo.lock;

  # Set Environment Variables
  RUST_BACKTRACE = "full";

  postInstall = ''
    install -Dm444 *.desktop -t $out/share/applications
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
