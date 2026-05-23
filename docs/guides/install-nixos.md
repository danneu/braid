# Install NixOS

You can follow NixOS' own guide here:

- <https://nixos.org/manual/nixos/stable/>
- <https://wiki.nixos.org/wiki/NixOS_Installation_Guide>

I'll document the process and the post-install setup, mostly for my own notes.

## Download NixOS image

- Go to <https://nixos.org/download/#nix-install-linux>
- Scroll down to ISO image section
- Download the **Graphical** 64-bit Intel/AMD image

This guide uses the graphical installer's wizard for partitioning, swap, and user creation. The Minimal ISO is out of scope here -- if you prefer it, follow [NixOS' install guide](https://nixos.org/manual/nixos/stable/) instead.

## Format USB stick with NixOS image

- Download [Etcher](https://etcher.balena.io/)
- Plug in USB stick
- Use Etcher to write your downloaded ISO image to your USB stick

## Install NixOS on NAS computer

- Plug in USB stick and boot from it
- Choose "Install NixOS (Linux LTS)" -- this launches the graphical installer
- Click through the wizard. It handles partitioning, swap, and user creation for you.
- Reboot when done; unplug the USB stick so it doesn't boot from it again

## Post-install

### Enable SSH

The graphical installer doesn't enable SSH by default. Log in physically on the NAS console with your user, then add openssh:

```sh
sudo nano /etc/nixos/configuration.nix
# add: services.openssh.enable = true;
sudo nixos-rebuild switch
```

Find the NAS's LAN IP and SSH in from your laptop:

```sh
# On NAS
ip a            # look for LAN ip address

# On your laptop
ssh dan@192.168.1.158

# Once logged in on NAS, change your password
passwd
```

The rest of this guide takes place over SSH from your laptop.

### Install vim

We'll add more packages later. For now I just want vim on the system to make the rest of the setup easier.

```sh
sudo nano /etc/nixos/configuration.nix

environment.systemPackages = with pkgs; [ vim ];

sudo nixos-rebuild switch
```

### Make git repo for NixOS config

The beauty of nix is that your OS is configured by git-diffable config files.

Instead of editing /etc/nixos/\*.nix files, I like to have a `~/world` git repo that tracks the NAS's nix config and push it to [danneu/world](https://github.com/danneu/world).

I'll name my NAS "nasbox" here.

```
~/world/
├── flake.nix
└── hosts/
    └── nasbox/                        # NAS (NixOS)
        ├── configuration.nix        # System config (boot, networking, services)
        ├── hardware-configuration.nix
        └── home.nix                 # User config (packages, shell, git, etc.)
```

Let's stub out that folder tree:

```sh
mkdir -p ~/world/hosts/nasbox
```

We use [home-manager](https://github.com/nix-community/home-manager) to manage user-level config (packages, git, shell, etc.) separately from the system config. This keeps `configuration.nix` lean — just boot, networking, and services — while `home.nix` handles everything specific to your user.

In `~/world/flake.nix`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    home-manager.url = "github:nix-community/home-manager/release-25.11";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, home-manager, ... }: {
    nixosConfigurations.nasbox = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./hosts/nasbox/configuration.nix
        home-manager.nixosModules.home-manager
        {
          home-manager.useGlobalPkgs = true;
          home-manager.useUserPackages = true;
          home-manager.users.dan = import ./hosts/nasbox/home.nix;
        }
      ];
    };
  };
}
```

Copy the generated NixOS config into your world repo:

```sh
cp /etc/nixos/configuration.nix ~/world/hosts/nasbox/
cp /etc/nixos/hardware-configuration.nix ~/world/hosts/nasbox/
```

Make sure `hosts/nasbox/configuration.nix` imports the hardware config with a relative path:

```nix
imports = [ ./hardware-configuration.nix ];
```

Create `hosts/nasbox/home.nix` for your user-level config:

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
    userName = "Your Name";
    userEmail = "your@email.com";
    extraConfig = {
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
sudo nixos-rebuild switch --flake ~/world#nasbox
```

From now on, you edit `~/world/` as your normal user and only `sudo` for the rebuild. System-level config goes in `configuration.nix`, user-level config goes in `home.nix`.

### Set up git and push to GitHub

Generate an SSH key on the NAS and add it to GitHub so you can push/pull:

```sh
ssh-keygen -t ed25519 -C "nasbox"
cat ~/.ssh/id_ed25519.pub
```

Copy the public key and add it at GitHub > Settings > SSH and GPG keys > [New SSH key](https://github.com/settings/ssh/new).

Then init and push:

```sh
cd ~/world
git init
git add -A
git commit -m "initial nixos config"
git remote add origin git@github.com:danneu/world.git
git push -u origin master
```

### Set hostname and pin the IP

Edit `~/world/hosts/nasbox/configuration.nix`:

```nix
networking.hostName = "nasbox";
```

```sh
sudo nixos-rebuild switch --flake ~/world#nasbox
```

For a stable IP, the simplest approach is a DHCP reservation on your router: look up the NAS's MAC address (`ip link show <iface>`) and tell the router to always hand it the same address. The reservation lives on the router, not the host -- no nix changes and no `nixos-rebuild` needed. Bonus: it survives interface renames.

If you'd rather pin it on the host, add to `configuration.nix`:

```nix
networking.interfaces.eno1.ipv4.addresses = [{
  address = "192.168.1.158";
  prefixLength = 24;
}];
networking.defaultGateway = "192.168.1.1";
networking.nameservers = [ "1.1.1.1" "8.8.8.8" ];
```

Then rebuild:

```sh
sudo nixos-rebuild switch --flake ~/world#nasbox
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

### Set up Claude Code and Codex

[numtide/llm-agents.nix](https://github.com/numtide/llm-agents.nix) is a daily-updated nix flake that packages 40+ AI coding agents, including Claude Code and OpenAI's Codex CLI. It exposes them via an overlay under `pkgs.llm-agents.*`.

Add it as a flake input in `~/world/flake.nix` and apply its overlay:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    home-manager.url = "github:nix-community/home-manager/release-25.11";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";

    # No `follows = "nixpkgs"` -- llm-agents is built against nixpkgs-unstable.
    llm-agents.url = "github:numtide/llm-agents.nix";
  };

  outputs = { nixpkgs, home-manager, llm-agents, ... }: {
    nixosConfigurations.nasbox = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./hosts/nasbox/configuration.nix
        { nixpkgs.overlays = [ llm-agents.overlays.default ]; }
        home-manager.nixosModules.home-manager
        {
          home-manager.useGlobalPkgs = true;
          home-manager.useUserPackages = true;
          home-manager.users.dan = import ./hosts/nasbox/home.nix;
        }
      ];
    };
  };
}
```

First, allow unfree packages in `hosts/nasbox/configuration.nix`:

```nix
nixpkgs.config.allowUnfree = true;
```

Then add both binaries to the existing `home.packages` list in `hosts/nasbox/home.nix`:

```nix
home.packages = with pkgs; [
  lazygit
  ripgrep
  fd
  jq
  htop
] ++ (with pkgs.llm-agents; [
    claude-code
    codex
  ]);
```

Rebuild:

```sh
sudo nixos-rebuild switch --flake ~/world#nasbox
```

Now you can run `claude` and `codex` from anywhere on the NAS.

#### Optional: use the numtide binary cache

By default, source-built agents like `codex` will compile locally on first install. To pull prebuilt binaries instead, add the numtide cache to your system config (in `hosts/nasbox/configuration.nix`):

```nix
nix.settings = {
  extra-substituters = [ "https://cache.numtide.com" ];
  extra-trusted-public-keys = [
    "niks3.numtide.com-1:DTx8wZduET09hRmMtKdQDxNNthLQETkc/yaX7M4qK0g="
  ];
};
```

Then rebuild. Subsequent installs will fetch from the cache.

### Next steps

At this point you have a working NixOS machine with SSH access, a stable IP, Claude Code + Codex, and a git-tracked config. Next: [add braid to your NixOS config](getting-started.md#install-the-nixos-module).
