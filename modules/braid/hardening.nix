{ }:
{
  # Shared systemd exec-sandbox baseline for braid units. This is the maximal
  # set of directives safe for every consumer; unit-specific write paths,
  # capabilities, address families, and device visibility stay at the call site.
  # See ADR 033.
  base = {
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    ProtectControlGroups = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    RestrictNamespaces = true;
    LockPersonality = true;
    MemoryDenyWriteExecute = true;
    SystemCallArchitectures = "native";
    RestrictSUIDSGID = true;
  };
}
