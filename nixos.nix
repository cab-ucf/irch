# NixOS: an irch storage node, always on, kept at the latest signed
# release. It keeps ciphertext for everyone who adds its code, and reads
# none of it.
#   imports = [ ./nixos.nix ];
#   services.irch.signers = ''you@lab namespaces="git" ssh-ed25519 AAAA…'';
#   services.irch.users = [ "you" ];     # their irch here pairs in one step
#   nixos-rebuild switch
#   systemctl start irch-update          # first build (then hourly)
#   journalctl -u irch | grep -A1 'Storage code'   # give this to members
# Needs rustc >= 1.88 (NixOS 25.11 or later).
{ config, lib, pkgs, ... }:
let
  signers = pkgs.writeText "irch-signers" config.services.irch.signers;
in {
  # Who may sign the release tags (ssh allowed_signers lines): a push to
  # the repository alone runs nothing here.
  options.services.irch.signers = lib.mkOption { type = lib.types.lines; };
  # Local accounts whose irch (app or terminal) may pair with this node
  # without the code being copied: it waits in /run/irch, theirs to read.
  options.services.irch.users = lib.mkOption {
    type = lib.types.listOf lib.types.str;
    default = [ ];
  };

  config = {
    users.users.irch = {
      isSystemUser = true; group = "irch";
      home = "/var/lib/irch"; createHome = true; homeMode = "700";
    };
    users.groups.irch.members = config.services.irch.users;

    # Build as irch, unprivileged; a new binary restarts the node (below).
    systemd.services.irch-update = {
      startAt = "hourly";
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      path = with pkgs; [ git openssh cargo rustc gcc ];
      serviceConfig = {
        Type = "oneshot"; User = "irch"; WorkingDirectory = "/var/lib/irch";
      };
      script = ''
        [ -d src/.git ] || git clone -q https://github.com/cab-ucf/irch src
        cd src && git fetch -q --force --tags
        tag=$(git tag -l 'v*' --sort=-v:refname | head -1)
        [ -n "$tag" ] && [ "$tag" != "$(cat ../deployed 2>/dev/null)" ] \
          || exit 0
        git -c gpg.format=ssh -c gpg.ssh.allowedSignersFile=${signers} \
          verify-tag "$tag"
        git -c advice.detachedHead=false checkout -q "$tag"
        cargo build -q --release --locked -p irch
        install -D target/release/irch ../bin/irch.new
        mv ../bin/irch.new ../bin/irch && echo "$tag" > ../deployed
      '';
    };
    systemd.paths.irch-new = {
      wantedBy = [ "multi-user.target" ];
      pathConfig.PathChanged = "/var/lib/irch/bin/irch";
    };
    systemd.services.irch-new.script = "systemctl restart irch";

    systemd.services.irch = {
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      unitConfig.ConditionPathExists = "/var/lib/irch/bin/irch";
      serviceConfig = {
        User = "irch";
        ExecStart = "/var/lib/irch/bin/irch --serve --dir /var/lib/irch/data";
        Restart = "always"; RestartSec = 5;
        NoNewPrivileges = true; PrivateTmp = true; PrivateDevices = true;
        ProtectSystem = "strict"; ProtectHome = true;
        StateDirectory = "irch/data";
        RuntimeDirectory = "irch"; RuntimeDirectoryMode = "0750";
        UMask = "0027";
        Environment = "IRCH_CODE_FILE=/run/irch/code";
        ProtectKernelTunables = true; ProtectControlGroups = true;
        RestrictSUIDSGID = true; LockPersonality = true;
        SystemCallArchitectures = "native";
      };
    };
  };
}
