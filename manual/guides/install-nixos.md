# Install NixOS

You can follow NixOS' own guide here:

- https://nixos158org/manual/nixos/stable/
- https://wiki.nixos.org/wiki/NixOS_Installation_Guide

I'll document the process and the post-install setup, mostly for my own notes.

## Download NixOS image

- Go to <https://nixos.org/download/#nix-install-linux>
- Scroll down to ISO image section
- Download Graphical or Minimal 64-bit Intel/AMD image

The graphical one is much nicer since it gives you an installation wizard GUI, lets you pick your target SSD from a dropdown, set up swap via checkbox.

## Format USB stick with NixOS image

- Download Etcher (https://etcher.balena.io/)
- Plug in USB stick
- Use Etcher to write your downloaded ISO image to your USB stick

## Install NixOS on NAS computer

- Plug in USB stick and boot from it.
- Choose "Install NixOS (Linux LTS)"
- You'll get dropped into a terminal

If you used the "Graphical" NixOS installer, you can skip the commands below until the "5. Edit config" part to set up your user.

If you used the "Minimal" NixOS ISO image, you'll need to do ran all the commands that the graphical installer would have done for you:

```sh
# 0. Find your SSD (probably nvme0n1, but verify)
lsblk

# 1. Partition (GPT)
sudo parted /dev/nvme0n1 -- mklabel gpt
## I'm setting up 8GB swap since that's how much RAM I have
sudo parted /dev/nvme0n1 -- mkpart root ext4 512MB -8GB
sudo parted /dev/nvme0n1 -- mkpart swap linux-swap -8GB 100%

sudo parted /dev/nvme0n1 -- mkpart ESP fat32 1MB 512MB
sudo parted /dev/nvme0n1 -- set 3 esp on

# 2. Format
sudo mkfs.ext4 -L nixos /dev/nvme0n1p1
sudo mkswap -L swap /dev/nvme0n1p2
sudo mkfs.fat -F 32 -n boot /dev/nvme0n1p3

# 3. Mount
sudo mount /dev/disk/by-label/nixos /mnt
sudo mkdir -p /mnt/boot
sudo mount -o umask=077 /dev/disk/by-label/boot /mnt/boot
sudo swapon /dev/nvme0n1p2

# 4. Generate config
sudo nixos-generate-config --root /mnt

# 5. Edit config (make sure systemd-boot is enabled)
sudo nano /mnt/etc/nixos/configuration.nix

# Add your user. Example:
#
# users.users.dan = {
#   isNormalUser = true;
#   extraGroups = [ "wheel" ];
#   initialPassword = "changeme"; # Something temporary just for first log-in
# };
#
# services.openssh.enable = true;

# 6. Install
sudo nixos-install

# 7. Reboot and unplug USB stick so it doesn't boot from it
sudo reboot
```

## Post-install

Since you set up a user and enabled sshd, you should be able to ssh into the NAS machine which is more comfortable.

```sh
# On NAS machine
ip a # look for LAN ip address

# e.g. On your laptop
ssh dan@192.168.1.158

# Once logged in on NAS, remember to change your password
passwd
```

### Install vim

We'll add more packages later. For now I just want vim on the system to make the rest of the setup easier.

```sh
sudo nano /etc/nixos/configuration.nix

environment.systemPackages = with pkgs; [ vim ];

sudo nixos-rebuild switch
```

The rest of this guide takes place on the NAS machine.

### Make git repo for NixOS config

The beauty of nix is that your OS is configured by git-diffable config files.

Instead of editing /etc/nixos/\*.nix files, I like to have a `~/world` git repo that contains the nix config for all my machines (this NAS machine, my MacBook), and I'll push it to https://github.com/danneu/world.

I'll name my NAS "caja" here.

```
~/world/
├── flake.nix
└── hosts/
    ├── caja/                        # NAS (NixOS)
    │   ├── configuration.nix        # System config (boot, networking, services)
    │   ├── hardware-configuration.nix
    │   └── home.nix                 # User config (packages, shell, git, etc.)
    └── mac/                         # MacBook (nix-darwin)
        └── ...
```

Let's stub out that folder tree:

```sh
mkdir -p ~/world/hosts/{caja,mac}
```

We use [home-manager](https://github.com/nix-community/home-manager) to manage user-level config (packages, git, shell, etc.) separately from the system config. This keeps `configuration.nix` lean — just boot, networking, and services — while `home.nix` handles everything specific to your user.

In `~/world/flake.nix`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    nix-darwin.url = "github:nix-darwin/nix-darwin";
    nix-darwin.inputs.nixpkgs.follows = "nixpkgs";
    home-manager.url = "github:nix-community/home-manager/release-25.11";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, nix-darwin, home-manager, ... }: {
    nixosConfigurations.caja = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./hosts/caja/configuration.nix
        home-manager.nixosModules.home-manager
        {
          home-manager.useGlobalPkgs = true;
          home-manager.useUserPackages = true;
          home-manager.users.dan = import ./hosts/caja/home.nix;
        }
      ];
    };

    darwinConfigurations.mac = nix-darwin.lib.darwinSystem {
      system = "aarch64-darwin";
      modules = [
        ./hosts/mac/configuration.nix
        home-manager.darwinModules.home-manager
        {
          home-manager.useGlobalPkgs = true;
          home-manager.useUserPackages = true;
          home-manager.users.dan = import ./hosts/mac/home.nix;
        }
      ];
    };
  };
}
```

Copy the generated NixOS config into your world repo:

```sh
cp /etc/nixos/configuration.nix ~/world/hosts/caja/
cp /etc/nixos/hardware-configuration.nix ~/world/hosts/caja/
```

Make sure `hosts/caja/configuration.nix` imports the hardware config with a relative path:

```nix
imports = [ ./hardware-configuration.nix ];
```

Create `hosts/caja/home.nix` for your user-level config:

```nix
{ pkgs, ... }:

{
  home.username = "dan";
  home.homeDirectory = "/home/dan";
  home.stateVersion = "25.11";
  programs.home-manager.enable = true;

  home.sessionVariables = {
    EDITOR = "vim";
    VISUAL = "vim";
  };

  programs.git = {
    enable = true;
    settings = {
      user.name = "Your Name";
      user.email = "your@email.com";
      init.defaultBranch = "master";
      pull.rebase = true;
      push.autoSetupRemote = true;
    };
  };

  home.packages = with pkgs; [
    lazygit   # Terminal UI for git
    ripgrep   # Fast recursive grep (rg)
    fd        # Fast find alternative
    jq        # JSON processor
    htop      # Interactive process viewer
  ];
}
```

Now rebuild from the flake instead of /etc/nixos:

```sh
sudo nixos-rebuild switch --flake ~/world#caja
```

From now on, you edit `~/world/` as your normal user and only `sudo` for the rebuild. System-level config goes in `configuration.nix`, user-level config goes in `home.nix`.

### Set up git and push to GitHub

Generate an SSH key on the NAS and add it to GitHub so you can push/pull:

```sh
ssh-keygen -t ed25519 -C "caja"
cat ~/.ssh/id_ed25519.pub
```

Copy the public key and add it at GitHub > Settings > SSH and GPG keys > New SSH key (https://github.com/settings/ssh/new).

Then init and push:

```sh
cd ~/world
git init
git add -A
git commit -m "initial nixos config"
git remote add origin git@github.com:danneu/world.git
git push -u origin master
```

### Set hostname and static IP

Edit `~/world/hosts/caja/configuration.nix`:

```nix
networking.hostName = "caja";

# Static IP (check your interface name with `ip link`)
networking.interfaces.eno1.ipv4.addresses = [{
  address = "192.168.1.158";
  prefixLength = 24;
}];
networking.defaultGateway = "192.168.1.1";
networking.nameservers = [ "1.1.1.1" "8.8.8.8" ];
```

```sh
sudo nixos-rebuild switch --flake ~/world#caja
```

### Set up SSH key auth

On your laptop, copy your public key to the NAS:

```sh
ssh-copy-id dan@192.168.1.158
```

Now you can SSH in without a password. Optionally disable password auth in `configuration.nix`:

```nix
services.openssh = {
  enable = true;
  settings.PasswordAuthentication = false;
};
```

### Set up Claude Code

[danneu/claude-code-nix](https://github.com/danneu/claude-code-nix) is an autoupdating nix flake for Claude Code. It provides a home-manager module.

Add it as a flake input in `~/world/flake.nix` and wire up its home-manager module via `sharedModules`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    nix-darwin.url = "github:nix-darwin/nix-darwin";
    nix-darwin.inputs.nixpkgs.follows = "nixpkgs";
    home-manager.url = "github:nix-community/home-manager/release-25.11";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";
    claude-code.url = "github:danneu/claude-code-nix";
  };

  outputs = { nixpkgs, nix-darwin, home-manager, claude-code, ... }: {
    nixosConfigurations.caja = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./hosts/caja/configuration.nix
        { nixpkgs.overlays = [ claude-code.overlays.default ]; }
        home-manager.nixosModules.home-manager
        {
          home-manager.useGlobalPkgs = true;
          home-manager.useUserPackages = true;
          home-manager.sharedModules = [
            claude-code.homeManagerModules.default
          ];
          home-manager.users.dan = import ./hosts/caja/home.nix;
        }
      ];
    };

    darwinConfigurations.mac = nix-darwin.lib.darwinSystem {
      system = "aarch64-darwin";
      modules = [
        ./hosts/mac/configuration.nix
        { nixpkgs.overlays = [ claude-code.overlays.default ]; }
        home-manager.darwinModules.home-manager
        {
          home-manager.useGlobalPkgs = true;
          home-manager.useUserPackages = true;
          home-manager.sharedModules = [
            claude-code.homeManagerModules.default
          ];
          home-manager.users.dan = import ./hosts/mac/home.nix;
        }
      ];
    };
  };
}
```

Then in `hosts/caja/home.nix`, enable Claude Code:

```nix
programs.claude-code = {
  enable = true;
};
```

Rebuild:

```sh
sudo nixos-rebuild switch --flake ~/world#caja
```

Now you can run `claude` from anywhere on the NAS.

### Next steps

At this point you have a working NixOS machine with SSH access, a static IP, Claude Code, and a git-tracked config. See [Getting Started](getting-started.md) to set up braid.
