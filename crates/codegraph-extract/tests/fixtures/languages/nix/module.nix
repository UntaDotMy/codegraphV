# A NixOS-style module touching every extraction path.
{ config, lib, pkgs, ... }@args:
let
  cfg = config.services.demo;
  helper = a: b: lib.concatStrings [ a b ];
  wrap = { name, value ? 1 }: "${name}=${toString value}";
  inherit (lib) mkIf mkOption types;
  inherit pkgs;
  long = "ééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééé";
in
{
  imports = [ ./hardware.nix ../common "./quoted.nix" ./dir/${args.x} ];
  options.services.demo.enable = mkOption { type = types.bool; default = false; };
  config = mkIf cfg.enable {
    environment.systemPackages = [ (pkgs.callPackage ./pkgs/tool.nix { }) pkgs.hello ];
    demoText = helper "a" (wrap { name = "x"; });
  };
  lib2 = import ./lib.nix { inherit lib; };
  other = builtins.import ../other;
  dyn = import (./dyn + "/x.nix");
  outputs = rec {
    nested = x: y: z: builtins.map (v: v + x) [ y z ];
    value = nested 1 2 3;
  };
}
