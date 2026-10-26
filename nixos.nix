# NixOS: irch for this computer's people, and an always-on storage node,
# both kept at irch's latest release: its source checked against irch's
# signing key, then built here, hourly.
#   imports = [ "${builtins.fetchTarball
#     "https://github.com/cab-ucf/irch/archive/main.tar.gz"}/nixos.nix" ];
#   (flakes: irch.nixosModules.default)
#   programs.irch.enable = true;      # the app, and `irch` in a terminal
#   services.irch.enable = true;      # a storage node, always on
#   services.irch.users = [ "you" ];  # their irch pairs with it in one step
#   journalctl -u irch | grep -A1 'Storage code'   # give this to others
{ config, lib, pkgs, ... }:
let
  cfg = config.services.irch;
  pinned = pkgs.callPackage ./package.nix { };
  latest = "https://github.com/cab-ucf/irch/releases";
  key = "RWSPfztwzR/MhakRL6cA70pN03n/nMCdTvoY5UHC2IcZuhcR+41RQI95";
  # The newest release built here, else the one this configuration pins.
  pick = bin: lib.hiPrio (pkgs.writeShellScriptBin bin ''
    b=/var/lib/irch/app/bin/${bin}
    [ -x "$b" ] || b=${pinned}/bin/${bin}
    exec "$b" "$@"
  '');
in {
  imports = [
    (lib.mkRemovedOptionModule [ "services" "irch" "signers" ]
      "irch's releases are checked against its own signing key now. For \
       the storage node: services.irch.enable = true.")
  ];

  options = {
    programs.irch.enable =
      lib.mkEnableOption "irch, the app and terminal client, kept up to date";
    services.irch.enable =
      lib.mkEnableOption "an always-on irch storage node, kept up to date";
    # Local accounts whose irch (app or terminal) may pair with this node
    # without the code being copied: it waits in /run/irch, theirs to read.
    services.irch.users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
    };
  };

  config = lib.mkIf (cfg.enable || config.programs.irch.enable) (lib.mkMerge [
    {
      users.users.irch = {
        isSystemUser = true; group = "irch"; home = "/var/lib/irch";
      };
      users.groups.irch.members = cfg.users;

      # Unprivileged: Nix builds it; nothing here runs unchecked source.
      systemd.services.irch-update = {
        startAt = "hourly";
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        path = with pkgs; [ curl jq minisign gnutar gzip coreutils nix ];
        environment.HOME = "/var/lib/irch";
        serviceConfig = {
          Type = "oneshot"; User = "irch"; WorkingDirectory = "/var/lib/irch";
          StateDirectory = "irch"; StateDirectoryMode = "0755";
        };
        script = ''
          v=$(curl -fsSL ${latest}/latest/download/latest.json | jq -r .version)
          [ "$v" != "$(cat built 2>/dev/null)" ] || exit 0
          at=${latest}/download/v$v
          curl -fsSL -o src.tar.gz "$at/source.tar.gz"
          curl -fsSL "$at/source.tar.gz.sig" | base64 -d > src.minisig
          minisign -Vqm src.tar.gz -x src.minisig -P ${key}
          rm -rf src && mkdir src && tar -xzf src.tar.gz -C src
          nix-build -o app -E \
            "(import ${pkgs.path} { }).callPackage $PWD/src/package.nix { }"
          rm -rf src src.tar.gz src.minisig
          echo "$v" > built
        '';
      };
      # Soon after it is first enabled, not just on the hour.
      systemd.timers.irch-update.timerConfig.OnBootSec = "2min";
    }

    (lib.mkIf config.programs.irch.enable {
      environment.systemPackages = [ pinned (pick "irch-app") (pick "irch") ];
    })

    (lib.mkIf cfg.enable {
      systemd.services.irch = {
        wantedBy = [ "multi-user.target" ];
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        serviceConfig = {
          User = "irch";
          ExecStart =
            "${pick "irch"}/bin/irch --serve --dir /var/lib/irch/data";
          Restart = "always"; RestartSec = 5;
          NoNewPrivileges = true; PrivateTmp = true; PrivateDevices = true;
          ProtectSystem = "strict"; ProtectHome = true;
          StateDirectory = "irch/data"; StateDirectoryMode = "0700";
          RuntimeDirectory = "irch"; RuntimeDirectoryMode = "0750";
          UMask = "0027";
          Environment = "IRCH_CODE_FILE=/run/irch/code";
          ProtectKernelTunables = true; ProtectControlGroups = true;
          RestrictSUIDSGID = true; LockPersonality = true;
          SystemCallArchitectures = "native";
        };
      };
      # A new release built: the node restarts on it.
      systemd.paths.irch-new = {
        wantedBy = [ "multi-user.target" ];
        pathConfig.PathChanged = "/var/lib/irch/built";
      };
      systemd.services.irch-new.script = "systemctl try-restart irch";
    })
  ]);
}
