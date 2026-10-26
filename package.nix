# irch from source: the app, and `irch` (the terminal client and storage
# node). nixos.nix builds each release with it.
{ lib, rustPlatform, pkg-config, wrapGAppsHook3, webkitgtk_4_1, libsoup_3
, glib-networking, gst_all_1, openssl, makeDesktopItem, copyDesktopItems }:
rustPlatform.buildRustPackage {
  pname = "irch";
  version = (lib.importTOML ./Cargo.toml).workspace.package.version;
  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;
  # typst asks for rustc 1.92 and builds with NixOS 25.11's 1.91.
  cargoBuildFlags = [ "-p" "irch" "-p" "irch-app" "--ignore-rust-version" ];
  buildFeatures = [ "irch-app/custom-protocol" ];
  # NixOS updates it: the app's own updater stands aside.
  env.IRCH_NIXOS = "1";
  doCheck = false; # its tests need a network
  nativeBuildInputs = [ pkg-config wrapGAppsHook3 copyDesktopItems ];
  buildInputs = [ webkitgtk_4_1 libsoup_3 glib-networking openssl ]
    ++ (with gst_all_1; [ gstreamer gst-plugins-base gst-plugins-good ]);
  desktopItems = [
    (makeDesktopItem {
      name = "irch";
      desktopName = "irch";
      comment = "Private, peer-to-peer chat";
      exec = "irch-app %u";
      icon = "irch";
      categories = [ "Network" "Chat" ];
      mimeTypes = [ "x-scheme-handler/irch" ];
    })
  ];
  postInstall = ''
    install -Dm644 app/icons/icon.png \
      $out/share/icons/hicolor/512x512/apps/irch.png
  '';
  meta = {
    description = "Private, peer-to-peer community chat over iroh";
    homepage = "https://github.com/cab-ucf/irch";
    license = lib.licenses.gpl3Plus;
    mainProgram = "irch-app";
  };
}
