# The app and `irch`; nixos.nix builds each release with this.
{ lib, rustPlatform, pkg-config, wrapGAppsHook3, webkitgtk_4_1, libsoup_3
, glib-networking, gst_all_1, openssl, makeDesktopItem, copyDesktopItems
, libayatana-appindicator, alsa-lib }:
rustPlatform.buildRustPackage {
  pname = "irch";
  version = (lib.importTOML ./Cargo.toml).workspace.package.version;
  # Not cargo's target/ (gigabytes).
  src = lib.cleanSourceWith {
    src = lib.cleanSource ./.;
    filter = p: _: baseNameOf p != "target";
  };
  # irpush comes from git, at the commit Cargo.lock names.
  cargoLock = { lockFile = ./Cargo.lock; allowBuiltinFetchGit = true; };
  # typst asks for rustc 1.92 and builds with NixOS 25.11's 1.91.
  cargoBuildFlags = [ "-p" "irch" "-p" "irch-app" "--ignore-rust-version" ];
  buildFeatures = [ "irch-app/custom-protocol" ];
  # NixOS updates it, not the app.
  env.IRCH_NIXOS = "1";
  doCheck = false; # its tests need a network
  nativeBuildInputs = [ pkg-config wrapGAppsHook3 copyDesktopItems ];
  # Calls' codecs through WebKit's GStreamer: Opus (base), VP8 and VP9
  # (good), AV1 and echo cancelling (bad), H.264 (libav).
  # A call's voice: ALSA, which PipeWire and PulseAudio serve too.
  buildInputs = [ webkitgtk_4_1 libsoup_3 glib-networking openssl alsa-lib ]
    ++ (with gst_all_1; [
      gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav
    ]);
  # The tray loads it at run time. WebKit's DMA-BUF renderer leaves a blank
  # window on some GPUs under Wayland (sway): off unless set otherwise.
  preFixup = ''
    gappsWrapperArgs+=(--prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ libayatana-appindicator ]})
    gappsWrapperArgs+=(--set-default WEBKIT_DISABLE_DMABUF_RENDERER 1)
  '';
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
