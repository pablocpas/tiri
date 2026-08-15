## Quick start

Use these commands to install tiri with [DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) for a fairly out-of-the-box experience.

Fedora:
```
sudo dnf copr enable pablocpas/tiri
sudo dnf copr enable avengemedia/dms
sudo dnf install tiri dms xwayland-satellite xdg-desktop-portal-gnome xdg-desktop-portal-gtk alacritty
systemctl --user add-wants tiri.service dms
```

Arch Linux (via [paru](https://github.com/morganamilo/paru)):
```
paru -S tiri
sudo pacman -Syu xwayland-satellite xdg-desktop-portal-gnome xdg-desktop-portal-gtk alacritty
paru -S dms-shell-bin matugen wl-clipboard cliphist cava qt6-multimedia-ffmpeg
systemctl --user add-wants tiri.service dms
```

Nix:
```
nix profile install github:pablocpas/tiri
```

After running these commands, log out, choose Tiri in your display manager, and log back in.
Or, if not using a display manager, run `tiri-session` on a TTY.

The default tiri config will run Waybar, so you might get two bars on screen.
To fix this, stop Waybar with `pkill waybar` command, then open `~/.config/tiri/config.kdl` and delete the `spawn-at-startup "waybar"` line.

Check the DankMaterialShell's [compositor setup page](https://danklinux.com/docs/dankmaterialshell/compositors#niri-configuration) to learn how to configure DMS-specific binds and other tiri integrations.
## Slower and more considered start

The easiest way to get tiri is to install one of the distribution packages (if available), or build from source.
See the [Building](#building) section if you'd like to compile tiri yourself and the [Packaging tiri](./Packaging-tiri.md) page if you want to package it.

After installing, start tiri from your display manager like GDM.
Press <kbd>Super</kbd><kbd>Return</kbd> to run a terminal ([Alacritty]) and <kbd>Super</kbd><kbd>D</kbd> to run an application launcher ([fuzzel]).
To exit tiri, press <kbd>Super</kbd><kbd>Shift</kbd><kbd>E</kbd>.

If you're not using a display manager, you should run `tiri-session` (systemd/dinit) or `tiri --session` (others) from a TTY.
The `--session` flag will make tiri import its environment variables globally into the system manager and D-Bus, and start its D-Bus services.
The `tiri-session` script will additionally start tiri as a systemd/dinit service, which starts up a graphical session target required by some services like portals.

You can also run `tiri` inside an existing desktop session.
Then it will open as a window, where you can give it a try.
Note that this windowed mode is mainly meant for development, so it is a bit buggy (in particular, there are issues with hotkeys).

Next, see the [list of important software](./Important-Software.md) required for normal desktop use, like a notification daemon and portals.
Also, check the [configuration introduction](./Configuration:-Introduction.md) page to get started configuring tiri.
There you can find links to other pages containing thorough documentation and examples for all options.
Finally, the [Xwayland](./Xwayland.md) page explains how to run X11 applications on tiri.

### Desktop environments

Some desktop environments and shells work with tiri/niri and can give a more out-of-the-box experience:

- [LXQt](https://lxqt-project.org/) officially supports niri, see [their wiki](https://github.com/lxqt/lxqt/wiki/ConfigWaylandSettings#general) for details on setting it up (may need adaptation for tiri).
- Many [XFCE](https://www.xfce.org/) components work on Wayland, including niri-based compositors. See [their wiki](https://wiki.xfce.org/releng/wayland_roadmap#component_specific_status) for details.
- There are complete desktop shells based on Quickshell that support niri, for example [DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) and [Noctalia](https://github.com/noctalia-dev/noctalia-shell).
- You can run a [COSMIC](https://system76.com/cosmic/) session with niri using [cosmic-ext-extra-sessions](https://github.com/Drakulix/cosmic-ext-extra-sessions).

### NVIDIA

The NVIDIA drivers currently have an issue with high VRAM usage due to a heap reuse quirk.
You're recommended to apply a manual fix documented [here](./Nvidia.md) if you run tiri on an NVIDIA GPU.

NVIDIA GPUs can have problems running tiri (for example, the screen remains black upon starting from a TTY).
Sometimes, the problems can be fixed.
You can try the following:

1. Update NVIDIA drivers. You need a GPU and drivers recent enough to support GBM.
2. Make sure kernel modesetting is enabled. This usually involves adding `nvidia-drm.modeset=1` to the kernel command line. Find and follow a guide for your distribution. Guides from other Wayland compositors can help.

### Asahi, ARM, and other kmsro devices

On some of these systems, tiri fails to correctly detect the primary render device.
If you're getting a black screen when starting tiri on a TTY, you can try to set the device manually.

First, find which devices you have:

```
$ ls -l /dev/dri/
drwxr-xr-x@       - root 14 мая 07:07 by-path
crw-rw----@   226,0 root 14 мая 07:07 card0
crw-rw----@   226,1 root 14 мая 07:07 card1
crw-rw-rw-@ 226,128 root 14 мая 07:07 renderD128
crw-rw-rw-@ 226,129 root 14 мая 07:07 renderD129
```

You will likely have one `render` device and two `card` devices.

Open the tiri config file at `~/.config/tiri/config.kdl` and put your `render` device path like this:

```kdl
debug {
    render-drm-device "/dev/dri/renderD128"
}
```

Save, then try to start tiri again.
If you still get a black screen, try using each of the `card` devices.

### Nix/NixOS

There's a common problem of mesa drivers going out of sync with tiri, so make sure your system mesa version matches the tiri mesa version.
When this happens, you usually see a black screen when trying to start tiri from a TTY.

Also, on Intel graphics, you may need a workaround described [here](https://wiki.nixos.org/wiki/Intel_Graphics).

### Virtual Machines

To run tiri in a VM, make sure to enable 3D acceleration.

## Main Default Hotkeys

When running on a TTY, the Mod key is <kbd>Super</kbd>.
When running in a window, the Mod key is <kbd>Alt</kbd>.

The default config follows the i3/sway convention for the core tiling actions: add <kbd>Shift</kbd> to a directional focus bind to move the focused container or window in that direction.

| Hotkey | Description |
| ------ | ----------- |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>/</kbd> | Show a list of important hotkeys |
| <kbd>Mod</kbd><kbd>Return</kbd> | Spawn `alacritty` (terminal) |
| <kbd>Mod</kbd><kbd>D</kbd> | Spawn `fuzzel` (application launcher) |
| <kbd>Mod</kbd><kbd>O</kbd> | Toggle overview |
| <kbd>Super</kbd><kbd>Alt</kbd><kbd>L</kbd> | Spawn `swaylock` (screen locker) |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>Q</kbd> | Close the focused window |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>-</kbd> | Move the focused window to the scratchpad |
| <kbd>Mod</kbd><kbd>-</kbd> | Show or hide the scratchpad window |
| **Navigation** | |
| <kbd>Mod</kbd><kbd>H</kbd> or <kbd>Mod</kbd><kbd>←</kbd> | Focus the tiling subtree to the left |
| <kbd>Mod</kbd><kbd>L</kbd> or <kbd>Mod</kbd><kbd>→</kbd> | Focus the tiling subtree to the right |
| <kbd>Mod</kbd><kbd>J</kbd> or <kbd>Mod</kbd><kbd>↓</kbd> | Focus the window below |
| <kbd>Mod</kbd><kbd>K</kbd> or <kbd>Mod</kbd><kbd>↑</kbd> | Focus the window above |
| <kbd>Mod</kbd><kbd>A</kbd> | Focus the parent container |
| <kbd>Mod</kbd><kbd>Home</kbd> | Focus the first tiling subtree |
| <kbd>Mod</kbd><kbd>End</kbd> | Focus the last tiling subtree |
| **Moving windows** | |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>H</kbd> or <kbd>Mod</kbd><kbd>Shift</kbd><kbd>←</kbd> | Move the focused tiling subtree to the left |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>L</kbd> or <kbd>Mod</kbd><kbd>Shift</kbd><kbd>→</kbd> | Move the focused tiling subtree to the right |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>J</kbd> or <kbd>Mod</kbd><kbd>Shift</kbd><kbd>↓</kbd> | Move the focused window down |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>K</kbd> or <kbd>Mod</kbd><kbd>Shift</kbd><kbd>↑</kbd> | Move the focused window up |
| **Monitors** | |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>Alt</kbd><kbd>H</kbd><kbd>J</kbd><kbd>K</kbd><kbd>L</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>Alt</kbd><kbd>←</kbd><kbd>↓</kbd><kbd>↑</kbd><kbd>→</kbd> | Focus the monitor in that direction |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>Shift</kbd><kbd>H</kbd><kbd>J</kbd><kbd>K</kbd><kbd>L</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>Shift</kbd><kbd>←</kbd><kbd>↓</kbd><kbd>↑</kbd><kbd>→</kbd> | Move the focused tiling subtree to the monitor in that direction |
| **Workspaces** | |
| <kbd>Mod</kbd><kbd>U</kbd> or <kbd>Mod</kbd><kbd>PageDown</kbd> | Switch to the workspace below |
| <kbd>Mod</kbd><kbd>I</kbd> or <kbd>Mod</kbd><kbd>PageUp</kbd> | Switch to the workspace above |
| <kbd>Mod</kbd><kbd>1</kbd>–<kbd>9</kbd> | Focus workspace by index |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>U</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>PageDown</kbd> | Move the focused tiling subtree to the workspace below |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>I</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>PageUp</kbd> | Move the focused tiling subtree to the workspace above |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>1</kbd>–<kbd>9</kbd> | Move the focused tiling subtree to workspace by index |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>U</kbd> or <kbd>Mod</kbd><kbd>Shift</kbd><kbd>PageDown</kbd> | Move the focused workspace down |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>I</kbd> or <kbd>Mod</kbd><kbd>Shift</kbd><kbd>PageUp</kbd> | Move the focused workspace up |
| **i3-style layout** | |
| <kbd>Mod</kbd><kbd>B</kbd> | Split horizontal |
| <kbd>Mod</kbd><kbd>V</kbd> | Split vertical |
| <kbd>Mod</kbd><kbd>W</kbd> | Set layout to tabbed |
| <kbd>Mod</kbd><kbd>S</kbd> | Set layout to stacked |
| <kbd>Mod</kbd><kbd>E</kbd> | Toggle split layout (horizontal/vertical) |
| <kbd>Mod</kbd><kbd>Space</kbd> | Switch focus between the floating and tiling layers |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>Space</kbd> | Toggle the focused window between floating and tiling |
| **Sizing and fullscreen** | |
| <kbd>Mod</kbd><kbd>R</kbd> | Cycle preset container width |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>R</kbd> | Cycle preset window height |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>R</kbd> | Reset window height |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>→</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>L</kbd> | Grow width |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>←</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>H</kbd> | Shrink width |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>↓</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>J</kbd> | Grow height |
| <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>↑</kbd> or <kbd>Mod</kbd><kbd>Ctrl</kbd><kbd>K</kbd> | Shrink height |
| <kbd>Mod</kbd><kbd>F</kbd> | Toggle full-screen on the focused window |
| **Screenshots** | |
| <kbd>PrtSc</kbd> | Take an area screenshot. Select the area to screenshot with mouse, then press Space to save the screenshot, or Escape to cancel |
| <kbd>Alt</kbd><kbd>PrtSc</kbd> | Take a screenshot of the focused window to clipboard and to `~/Pictures/Screenshots/` |
| <kbd>Ctrl</kbd><kbd>PrtSc</kbd> | Take a screenshot of the focused monitor to clipboard and to `~/Pictures/Screenshots/` |
| **Session** | |
| <kbd>Mod</kbd><kbd>Escape</kbd> | Toggle keyboard shortcuts inhibitor |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>P</kbd> | Power off monitors |
| <kbd>Mod</kbd><kbd>Shift</kbd><kbd>E</kbd> or <kbd>Ctrl</kbd><kbd>Alt</kbd><kbd>Delete</kbd> | Exit tiri |

> [!NOTE]
> Several actions still use `column` in their names for compatibility with niri and the IPC/config surface.
> In current tiri, these actions operate on the top-level tiling subtree or container on a workspace rather than on a special internal "column" type.

## Building

First, install the dependencies for your distribution.

- Ubuntu 24.04:

    ```sh
    sudo apt-get install -y gcc clang libudev-dev libgbm-dev libxkbcommon-dev libegl1-mesa-dev libwayland-dev libinput-dev libdbus-1-dev libsystemd-dev libseat-dev libpipewire-0.3-dev libpango1.0-dev libdisplay-info-dev
    ```

- Fedora:

    ```sh
    sudo dnf install gcc libudev-devel libgbm-devel libxkbcommon-devel wayland-devel libinput-devel dbus-devel systemd-devel libseat-devel pipewire-devel pango-devel cairo-gobject-devel clang libdisplay-info-devel
    ```

Next, get latest stable Rust: https://rustup.rs/

Then, build tiri with `cargo build --release`.

Check Cargo.toml for a list of build features.
For example, you can replace systemd integration with dinit integration using `cargo build --release --no-default-features --features dinit,dbus,xdp-gnome-screencast`.

> [!WARNING]
> Do NOT build with `--all-features`!
>
> Some features are meant only for development use.
> For example, one of the features enables collection of profiling data into a memory buffer that will grow indefinitely until you run out of memory.

### NixOS/Nix

We have a community-maintained flake which provides a devshell with required dependencies. Use `nix build` to build tiri, and then run `./results/bin/tiri`.

If you're not on NixOS, you may need [NixGL](https://github.com/nix-community/nixGL) to run the resulting binary:

```sh
nix run --impure github:guibou/nixGL -- ./results/bin/tiri
```

### Manual Installation

If installing directly without a package, the recommended file destinations are slightly different.
In this case, put the files in the directories indicated in the table below.
These may vary depending on your distribution.

Don't forget to make sure that the path to `tiri` in tiri.service is correct.
This defaults to `/usr/bin/tiri`.

| File | Destination |
| ---- | ----------- |
| `target/release/tiri` | `/usr/local/bin/` |
| `resources/tiri-session` | `/usr/local/bin/` |
| `resources/tiri.desktop`  | `/usr/local/share/wayland-sessions/` |
| `resources/tiri-portals.conf` | `/usr/local/share/xdg-desktop-portal/` |
| `resources/profiles/i3.kdl` | `/usr/local/share/tiri/profiles/` |
| `resources/tiri.service` (systemd) | `/etc/systemd/user/` |
| `resources/tiri-shutdown.target` (systemd) | `/etc/systemd/user/` |
| `resources/dinit/tiri` (dinit) | `/etc/dinit.d/user/` |
| `resources/dinit/tiri.target` (dinit) | `/etc/dinit.d/user/` |

[Alacritty]: https://github.com/alacritty/alacritty
[fuzzel]: https://codeberg.org/dnkl/fuzzel
