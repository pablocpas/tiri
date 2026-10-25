This page contains various bits of information helpful for integrating tiri in a distribution.
First, for creating a tiri package, see the [Packaging](./Packaging-tiri.md) page.

### Configuration

Tiri will load configuration from `$XDG_CONFIG_HOME/tiri/config.kdl` or `~/.config/tiri/config.kdl`, falling back to `/etc/tiri/config.kdl`.
If both of these files are missing, tiri will create `$XDG_CONFIG_HOME/tiri/config.kdl` with the contents of [the default configuration file](https://github.com/pablocpas/tiri/blob/main/resources/default-config.kdl), which are embedded into the tiri binary at build time.
It will also create `$XDG_CONFIG_HOME/tiri/profiles/i3.kdl`, an optional i3/sway-style profile that users can enable with `include "profiles/i3.kdl"` at the end of their config.

This means that you can customize your distribution defaults by creating `/etc/tiri/config.kdl`.
When this file is present, tiri *will not* automatically create a config at `~/.config/tiri/`, so you'll need to direct your users how to do it themselves.

Keep in mind that we update the default config in new releases, so if you have a custom `/etc/tiri/config.kdl`, you likely want to inspect and apply the relevant changes too.

The default configuration locations can be overridden with the `TIRI_CONFIG` environment variable.
You can also change the configuration path at runtime via the tiri IPC or using the command `tiri msg action load-config-file --path <path-to-config.kdl>`.

You can split the tiri config file into multiple files using [`include`](./Configuration:-Include.md).

### Xwayland

Xwayland is required for running X11 apps and games, and also the Orca screen reader.

<sup>Upstream niri: 25.08</sup> Tiri integrates with [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) out of the box.
The integration requires xwayland-satellite >= 0.7 available in `$PATH`.
Please consider making tiri depend on (or at least recommend) the xwayland-satellite package.
If you had a custom config which manually started `xwayland-satellite` and set `$DISPLAY`, you should remove those customizations for the automatic integration to work.

You can change the path where tiri looks for xwayland-satellite using the [`xwayland-satellite` top-level option](./Configuration:-Miscellaneous.md#xwayland-satellite).

### Keyboard layout

<sup>Upstream niri: 25.08</sup> By default (unless [manually configured](./Configuration:-Input.md#layout) otherwise), tiri reads keyboard layout settings from systemd-localed at `org.freedesktop.locale1` over D-Bus.
Make sure your system installer sets the keyboard layout via systemd-localed, and tiri should pick it up.

### Autostart

Tiri works with the normal systemd autostart.
The default [tiri.service](https://github.com/pablocpas/tiri/blob/main/resources/tiri.service) brings up `graphical-session.target` as well as `xdg-desktop-autostart.target`.

To make a program run at tiri startup without editing the tiri config, you can either link its .desktop to `~/.config/autostart/`, or use a .service file with `WantedBy=graphical-session.target`.
See the [example systemd setup](./Example-systemd-Setup.md) page for some examples.

If this is inconvenient, you can also add [`spawn-at-startup`](./Configuration:-Miscellaneous.md#spawn-at-startup) lines in the tiri config.

### Screen readers

<sup>Upstream niri: 25.08</sup> Tiri works with the [Orca](https://orca.gnome.org) screen reader.
Please see the [Accessibility](./Accessibility.md) page for details and advice for accessibility-focused distributions.

### Desktop components

You very likely want to run at least a notification daemon, portals, and an authentication agent.
This is detailed on the [Important Software](./Important-Software.md) page.

On top of that, you may want to preconfigure some desktop shell components to make the experience less barebones.
Tiri's default config spawns [Waybar](https://github.com/Alexays/Waybar), which is a good starting point, but you may want to consider changing its default configuration to be less of a kitchen sink, and adding the `tiri/workspaces` module.
You will probably also want a desktop background tool ([swaybg](https://github.com/swaywm/swaybg) or [awww (which used to be swww)](https://codeberg.org/LGFae/awww/)), and a nicer screen locker (compared to the default `swaylock`), like [hyprlock](https://github.com/hyprwm/hyprlock/).

Alternatively, some desktop environments and shells work with tiri, and can give a more cohesive experience in one package:

- [LXQt](https://lxqt-project.org/) officially supports tiri, see [their wiki](https://lxqt-project.org/wiki/Wayland-Session) for details on setting it up.
- Many [XFCE](https://www.xfce.org/) components work on Wayland, including tiri. See [their wiki](https://wiki.xfce.org/releng/wayland_roadmap#component_specific_status) for details.
- There are complete desktop shells that support tiri, for example [DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) and [Noctalia](https://github.com/noctalia-dev/noctalia-shell).
- You can run a [COSMIC](https://system76.com/cosmic/) session with tiri using [cosmic-ext-extra-sessions](https://github.com/Drakulix/cosmic-ext-extra-sessions).
