# NixOS: an irch storage node, always on, kept at the latest commit hourly.
# It keeps ciphertext for everyone who adds its code, and reads none of it.
#   imports = [ ./nixos.nix ];  nixos-rebuild switch
#   systemctl start irch-update          # first build (then hourly)
#   journalctl -u irch | grep -A1 'Storage code'   # give this to members
# Needs rustc >= 1.88 (NixOS 25.11 or later).
{ pkgs, ... }: {
  users.users.irch = {
    isSystemUser = true; group = "irch";
    home = "/var/lib/irch"; createHome = true; homeMode = "700";
  };
  users.groups.irch = { };

  # Build as irch, unprivileged; a new binary restarts the node (below).
  systemd.services.irch-update = {
    startAt = "hourly";
    wants = [ "network-online.target" ]; after = [ "network-online.target" ];
    path = with pkgs; [ git cargo rustc gcc ];
    serviceConfig = {
      Type = "oneshot"; User = "irch"; WorkingDirectory = "/var/lib/irch";
    };
    script = ''
      [ -d src/.git ] || git clone -q https://github.com/cab-ucf/irch src
      cd src && git pull -q --ff-only
      [ "$(git rev-parse HEAD)" = "$(cat ../deployed 2>/dev/null)" ] && exit
      cargo build -q --release --locked -p irch
      install -D target/release/irch ../bin/irch.new
      mv ../bin/irch.new ../bin/irch && git rev-parse HEAD > ../deployed
    '';
  };
  systemd.paths.irch-new = {
    wantedBy = [ "multi-user.target" ];
    pathConfig.PathChanged = "/var/lib/irch/bin/irch";
  };
  systemd.services.irch-new.script = "systemctl restart irch";

  systemd.services.irch = {
    wantedBy = [ "multi-user.target" ];
    wants = [ "network-online.target" ]; after = [ "network-online.target" ];
    unitConfig.ConditionPathExists = "/var/lib/irch/bin/irch";
    serviceConfig = {
      User = "irch";
      ExecStart = "/var/lib/irch/bin/irch --serve --dir /var/lib/irch/data";
      Restart = "always"; RestartSec = 5;
      NoNewPrivileges = true; PrivateTmp = true; PrivateDevices = true;
      ProtectSystem = "strict"; ProtectHome = true;
      StateDirectory = "irch/data";
      ProtectKernelTunables = true; ProtectControlGroups = true;
      RestrictSUIDSGID = true; LockPersonality = true;
      SystemCallArchitectures = "native";
    };
  };
}
