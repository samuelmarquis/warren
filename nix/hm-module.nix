# home-manager module: `programs.warren`.
#
# Installs warren and, if `hosts` is set, writes ~/.warren/hosts from it. The
# file becomes a symlink into the store; warren notices a rebuild that changes
# it the same as a hand edit. Leave `hosts` empty to keep editing it by hand.
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.warren;
  hostType = lib.types.either lib.types.str (lib.types.submodule {
    options = {
      dest = lib.mkOption {
        type = lib.types.str;
        description = "ssh destination (anything `ssh` accepts, including a Host alias).";
        example = "programchild@leilan";
      };
      warren = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Where warren lives on that machine. Needed when a non-interactive
          ssh command there would not find `warren` on PATH.
        '';
        example = "/Users/me/.local/bin/warren";
      };
    };
  });
  line = h:
    if builtins.isString h then h
    else h.dest + lib.optionalString (h.warren != null) "  ${h.warren}";
in
{
  options.programs.warren = {
    enable = lib.mkEnableOption "warren, a dashboard for a colony of Claude Code / OMP agents";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.warren;
      defaultText = lib.literalExpression "warren.packages.\${system}.warren";
      description = "The warren package to install.";
    };
    hosts = lib.mkOption {
      type = lib.types.listOf hostType;
      default = [ ];
      description = "Other machines whose agents join this sidebar, in order (~/.warren/hosts).";
      example = lib.literalExpression ''[ "smq" { dest = "leilan"; warren = "/etc/profiles/per-user/programchild/bin/warren"; } ]'';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    home.file.".warren/hosts" = lib.mkIf (cfg.hosts != [ ]) {
      text = "# Managed by home-manager (programs.warren.hosts).\n"
        + lib.concatMapStrings (h: line h + "\n") cfg.hosts;
    };
  };
}
