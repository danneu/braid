{ lib }:
{
  # Keep these predicates in lockstep with cli/src/types.rs parse functions.
  mountPointOk =
    path:
    let
      mp = toString path;
      trimmed =
        if mp != "/" && lib.hasSuffix "/" mp then
          builtins.substring 0 (builtins.stringLength mp - 1) mp
        else
          mp;
      segs = lib.splitString "/" trimmed;
      body = builtins.tail segs;
      segOk = s: s != "" && s != "." && s != ".." && builtins.match "[A-Za-z0-9_.-]+" s != null;
    in
    lib.hasPrefix "/" trimmed && builtins.head segs == "" && body != [ ] && builtins.all segOk body;

  isValidUpsName =
    name:
    name != ""
    && builtins.stringLength name <= 32
    && !(lib.hasPrefix "-" name)
    && builtins.match "[A-Za-z0-9._-]+" name != null;

  isValidInterface =
    iface:
    iface != ""
    && builtins.stringLength iface <= 15
    && iface != "."
    && iface != ".."
    && !(lib.hasPrefix "-" iface)
    && builtins.match "[A-Za-z0-9._-]+" iface != null;
}
