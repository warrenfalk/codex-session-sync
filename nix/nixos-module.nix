{ self }:
{
  lib,
  pkgs,
  config,
  ...
}:
let
  cfg = config.services.codex-session-sync;
  package = cfg.package;
  execArgs = [
    "${package}/bin/codex-session-sync"
    "daemon"
    "--root"
    cfg.sessionsRoot
    "--state-db"
    cfg.stateDb
    "--spool-dir"
    cfg.spoolDir
    "--repo"
    cfg.repoPath
    "--remote"
    cfg.remote
    "--branch"
    cfg.branch
    "--interval-secs"
    (toString cfg.intervalSeconds)
  ] ++ lib.optional (!cfg.push) "--no-push";
in
{
  options.services.codex-session-sync = {
    enable = lib.mkEnableOption "Codex session sync user service";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.default;
      defaultText = lib.literalExpression "self.packages.\${pkgs.system}.default";
      description = "Package providing the codex-session-sync binary.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      example = "alice";
      description = "User account whose systemd user manager should run the sync service.";
    };

    sessionsRoot = lib.mkOption {
      type = lib.types.str;
      default = "%h/.codex/sessions";
      description = "Path to the Codex sessions directory, interpreted by systemd with user specifiers.";
    };

    stateDb = lib.mkOption {
      type = lib.types.str;
      default = "%h/.local/state/codex-session-sync/state.sqlite3";
      description = "Path to the SQLite state database, interpreted by systemd with user specifiers.";
    };

    spoolDir = lib.mkOption {
      type = lib.types.str;
      default = "%h/.local/state/codex-session-sync/spool";
      description = "Path to the local spool directory, interpreted by systemd with user specifiers.";
    };

    repoPath = lib.mkOption {
      type = lib.types.str;
      default = "%h/.local/share/codex-session-sync/repo";
      description = "Path to the writable local clone of the central sync repository, interpreted by systemd with user specifiers.";
    };

    remote = lib.mkOption {
      type = lib.types.str;
      default = "origin";
      description = "Git remote name used for pull/push.";
    };

    branch = lib.mkOption {
      type = lib.types.str;
      default = "main";
      description = "Git branch used for pull/push.";
    };

    intervalSeconds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 10;
      description = "Polling interval for the daemon loop.";
    };

    push = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether the daemon should push after committing imported batches.";
    };

    logFilter = lib.mkOption {
      type = lib.types.str;
      default = "codex_session_sync=info";
      description = "RUST_LOG filter passed to the service.";
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Additional environment variables for the user service.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.codex-session-sync = {
      description = "Sync Codex sessions into a central Git repository";
      wantedBy = [ "default.target" ];
      after = [ "default.target" ];
      unitConfig = {
        ConditionUser = cfg.user;
      };
      serviceConfig = {
        ExecStart = lib.concatStringsSep " " (map lib.escapeShellArg execArgs);
        Restart = "on-failure";
        RestartSec = "15s";
      };
      environment = {
        RUST_LOG = cfg.logFilter;
      } // cfg.extraEnvironment;
    };
  };
}
