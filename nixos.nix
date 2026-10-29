# Imported, the app and a storage node at irch's latest release: its source
# checked against irch's signing key, then built here, hourly. Everyone with
# a home in /home pairs their irch with the node in one step. To leave out:
#   programs.irch.enable = false;     # the app, and `irch` in a terminal
#   services.irch.enable = false;     # the storage node, always on
#   sudo cat /run/irch/code           # the storage code, for others
{ config, lib, pkgs, ... }:
let
  cfg = config.services.irch;
  pinned = pkgs.callPackage ./package.nix { };
  latest = "https://github.com/cab-ucf/irch/releases";
  key = "RWSPfztwzR/MhakRL6cA70pN03n/nMCdTvoY5UHC2IcZuhcR+41RQI95";
  # The newest release built here, else the pinned one.
  pick = bin: lib.hiPrio (pkgs.writeShellScriptBin bin ''
    b=/var/lib/irch-app/app/bin/${bin}
    [ -x "$b" ] || b=${pinned}/bin/${bin}
    exec "$b" "$@"
  '');
in {
  options = {
    programs.irch.enable = lib.mkEnableOption
      "irch, the app and terminal client, kept up to date" // { default = true; };
    # The app at login, in the tray.
    programs.irch.autostart = lib.mkOption {
      type = lib.types.bool;
      default = true;
    };
    services.irch.enable = lib.mkEnableOption
      "an always-on irch storage node, kept up to date" // { default = true; };
    # Accounts that read the node's code in /run/irch, to pair unasked.
    services.irch.users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = lib.attrNames (lib.filterAttrs
        (_: u: u.isNormalUser && lib.hasPrefix "/home/" u.home) config.users.users);
      defaultText = "everyone with a home in /home";
    };
    services.irch.signers = lib.mkOption {
      type = lib.types.lines; default = ""; visible = false;
    };
  };

  config = lib.mkIf (cfg.enable || config.programs.irch.enable) (lib.mkMerge [
    {
      warnings = lib.optional (cfg.signers != "")
        "services.irch.signers does nothing now (irch's own key checks releases): delete it";
      users.users.irch = {
        isSystemUser = true; group = "irch"; home = "/var/lib/irch";
      };
      users.groups.irch.members = cfg.users;
      # Apart from the node: taken over, it cannot swap what all here run.
      users.users.irch-update = {
        isSystemUser = true; group = "irch-update"; home = "/var/lib/irch-app";
      };
      users.groups.irch-update = { };

      # Only source signed as this very release, never an older one.
      systemd.services.irch-update = {
        startAt = "hourly";
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        path = with pkgs; [ curl jq minisign gnutar gzip coreutils gnused nix ];
        environment.HOME = "/var/lib/irch-app";
        serviceConfig = {
          Type = "oneshot"; User = "irch-update";
          WorkingDirectory = "/var/lib/irch-app";
          StateDirectory = "irch-app"; StateDirectoryMode = "0755";
        };
        script = ''
          v=$(curl -fsSL ${latest}/latest/download/latest.json | jq -r .version)
          case $v in "" | *[!0-9.]*) echo "no version: $v"; exit 1 ;; esac
          old=$(cat built 2>/dev/null || echo 0)
          [ "$(printf '%s\n' "$old" "$v" | sort -V | tail -1)" = "$v" ] &&
            [ "$v" != "$old" ] || exit 0
          at=${latest}/download/v$v
          curl -fsSL -o src.tar.gz "$at/source.tar.gz"
          curl -fsSL "$at/source.tar.gz.sig" | base64 -d > src.minisig
          said=$(minisign -Vm src.tar.gz -x src.minisig -P ${key} |
            sed -n 's/^Trusted comment: //p')
          t=$(printf '\t')
          case $said in
            *"file:source.tar.gz''${t}version:$v") ;;
            *) echo "signed, but not as v$v's source: $said"; exit 1 ;;
          esac
          rm -rf src && mkdir src && tar -xzf src.tar.gz -C src
          nix-build -o app -E "(import ${pkgs.path} {
            config = { }; overlays = [ ]; }).callPackage $PWD/src/package.nix { }"
          rm -rf src src.tar.gz src.minisig
          echo "$v" > built
        '';
      };
      # Soon after it is enabled, not just on the hour.
      systemd.timers.irch-update.timerConfig.OnBootSec = "2min";
    }

    (lib.mkIf config.programs.irch.enable {
      environment.systemPackages = [ pinned (pick "irch-app") (pick "irch") ];
      environment.etc."xdg/autostart/irch.desktop" =
        lib.mkIf config.programs.irch.autostart {
          text = ''
            [Desktop Entry]
            Type=Application
            Name=irch
            Exec=irch-app --hidden
            NoDisplay=true
          '';
        };
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
          # No privileges, no other processes, no new code in memory; only
          # the network (netlink: iroh watches the interfaces).
          CapabilityBoundingSet = ""; AmbientCapabilities = "";
          ProtectProc = "invisible"; ProcSubset = "pid";
          ProtectKernelModules = true; ProtectKernelLogs = true;
          ProtectClock = true; ProtectHostname = true;
          RestrictNamespaces = true; RestrictRealtime = true;
          MemoryDenyWriteExecute = true;
          RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
        };
      };
      systemd.paths.irch-new = {
        wantedBy = [ "multi-user.target" ];
        pathConfig.PathChanged = "/var/lib/irch-app/built";
      };
      systemd.services.irch-new.script = "systemctl try-restart irch";
    })
  ]);
}
