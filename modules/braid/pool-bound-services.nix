# Pool-lifecycle consumer stamping -- braid.poolBoundServices.
{
  config,
  lib,
  options,
  ...
}:
let
  cfg = config.braid;
  knownUnitSuffixes = [
    ".automount"
    ".device"
    ".mount"
    ".path"
    ".scope"
    ".service"
    ".slice"
    ".socket"
    ".swap"
    ".target"
    ".timer"
  ];

  hasKnownUnitSuffix = name: lib.any (suffix: lib.hasSuffix suffix name) knownUnitSuffixes;
  badUnitNames = builtins.filter hasKnownUnitSuffix cfg.poolBoundServices;
  internalNames = builtins.filter (name: name == "braid-online") cfg.poolBoundServices;
  stampNames = builtins.filter (
    name: !hasKnownUnitSuffix name && name != "braid-online"
  ) cfg.poolBoundServices;

  definitionContains =
    name: definition:
    let
      value = definition.value or definition;
    in
    builtins.isAttrs value && builtins.hasAttr name value;

  serviceDefinitionCount =
    name: lib.count (definitionContains name) options.systemd.services.definitions;

  undefinedServiceNames = builtins.filter (name: serviceDefinitionCount name <= 1) stampNames;
in
{
  options.braid.poolBoundServices = lib.mkOption {
    type = lib.types.listOf lib.types.str;
    default = [ ];
    example = [
      "samba-smbd"
      "nfs-server"
    ];
    description = ''
      Long-running systemd services to bind to the pool lifecycle, as bare
      NixOS service names (the systemd.services.<name> key) -- "samba-smbd",
      not "samba-smbd.service".

      Each listed service gets the consumer contract from ADR 018: wantedBy +
      bindsTo + after braid-online.service, plus ConditionPathIsMountPoint on
      braid.mountPoint. It starts after `braid unlock` brings the pool online
      and stops before `braid lock` unmounts. Stamping is append-only: the
      service keeps its existing boot edges, and the condition turns premature
      boot starts into a clean skip while the pool is locked.

      Do not list timer-driven oneshot jobs (backups) -- the wantedBy edge
      would run them on every unlock; give those ConditionPathIsMountPoint
      only. A service consuming a subvolume through a dedicated mount unit
      binds to that mount unit instead -- see the mounting-subvolumes guide.
    '';
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = badUnitNames == [ ];
        message = "braid.poolBoundServices entries must be bare NixOS service names without systemd unit suffixes. Got: ${builtins.toJSON badUnitNames}.";
      }
      {
        assertion = internalNames == [ ];
        message = "braid.poolBoundServices must not include braid-online; braid owns braid-online.service as the pool lifecycle marker.";
      }
      {
        assertion = undefinedServiceNames == [ ];
        message = "braid.poolBoundServices entries must name services defined by another NixOS module before braid stamps lifecycle edges. Missing: ${builtins.toJSON undefinedServiceNames}.";
      }
    ];

    systemd.services = lib.genAttrs stampNames (_name: {
      wantedBy = [ "braid-online.service" ];
      bindsTo = [ "braid-online.service" ];
      after = [ "braid-online.service" ];
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
    });
  };
}
