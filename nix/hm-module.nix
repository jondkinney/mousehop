self: {
  config,
  pkgs,
  lib,
  ...
}:
with lib; let
  cfg = config.programs.mousehop;
  defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  tomlFormat = pkgs.formats.toml {};
in {
  options.programs.mousehop = with types; {
    enable = mkEnableOption "Whether or not to enable mousehop.";
    package = mkOption {
      type = with types; nullOr package;
      default = defaultPackage;
      defaultText = literalExpression "inputs.mousehop.packages.${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The mousehop package to use.

        By default, this option will use the `packages.default` as exposed by this flake.
      '';
    };
    systemd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.isLinux;
      description = "Whether to enable to systemd service for mousehop on linux.";
    };
    launchd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.isDarwin;
      description = "Whether to enable to launchd service for mousehop on macOS.";
    };
    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = {};
      example = builtins.fromTOML (builtins.readFile (self + /config.toml));
      description = ''
        Optional configuration written to {file}`$XDG_CONFIG_HOME/mousehop/config.toml`.

        See <https://github.com/jondkinney/mousehop/> for
        available options and documentation.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.user.services.mousehop = lib.mkIf cfg.systemd {
      Unit = {
        Description = "Systemd service for Mousehop";
        Requires = ["graphical-session.target"];
      };
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/mousehop daemon";
      };
      Install.WantedBy = [
        (lib.mkIf config.wayland.windowManager.hyprland.systemd.enable "hyprland-session.target")
        (lib.mkIf config.wayland.windowManager.sway.systemd.enable "sway-session.target")
      ];
    };

    launchd.agents.mousehop = lib.mkIf cfg.launchd {
      enable = true;
      config = {
        ProgramArguments = [
          "${cfg.package}/bin/mousehop"
          "daemon"
        ];
        KeepAlive = true;
      };
    };

    home.packages = [
      cfg.package
    ];

    xdg.configFile."mousehop/config.toml" = lib.mkIf (cfg.settings != {}) {
      source = tomlFormat.generate "config.toml" cfg.settings;
    };
  };
}
