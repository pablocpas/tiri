### Overview

There are several gestures in tiri.

Also see the [gestures configuration](./Configuration:-Gestures.md) wiki page.

### Mouse

#### Interactive move

You can move windows by holding <kbd>Mod</kbd> and the left mouse button.

You can customize the look of the window insertion preview in the [`insert-hint` layout config](./Configuration:-Layout.md#insert-hint).

While a move is in progress, clicking the other mouse button toggles the moved window between tiling and floating.

#### Interactive resize

You can resize windows by holding <kbd>Mod</kbd> and the right mouse button.

#### Reset window height

If you double-click on a top or bottom tiled window resize edge, the window height will reset to automatic.

This works with both window-initiated resizes (when using client-side decorations), and tiri-initiated <kbd>Mod</kbd> + right click resizes.

#### Toggle full width

If you double-click on a left or right tiled window resize edge, the column will expand to the full workspace width.

This works with both window-initiated resizes (when using client-side decorations), and tiri-initiated <kbd>Mod</kbd> + right click resizes.

#### Workspace switch

Switch workspaces by holding <kbd>Mod</kbd> and the middle mouse button (or the wheel) and dragging the mouse vertically.

### Touchpad

#### Workspace switch

Switch workspaces with three-finger vertical swipes.

In the inherited niri configuration model, horizontal touchpad movement can be used for view-offset gestures.
In current tiri's i3/sway-style tiling layout, the reliable touchpad gesture to document is the vertical workspace switch.

#### Open and close the overview

Open and close the overview with a four-finger vertical swipe.

### Touchscreen

#### Interactive Move

<sup>Since: 0.1.10</sup>

You can move windows by holding <kbd>Mod</kbd> and dragging the window with your finger.

Dragging a tiled window horizontally will scroll the view instead.

You can customize the look of the window insertion preview in the [`insert-hint` layout config](./Configuration:-Layout.md#insert-hint).

<sup>Since: 25.11</sup> Tap with a second finger while moving to toggle between floating and tiling layout to put the window into.

### Tablet

#### Interactive Move

<sup>Since: next release</sup>

You can move windows by holding <kbd>Mod</kbd> and dragging the window with a tablet pen.

Dragging a tiled window horizontally will scroll the view instead.

You can customize the look of the window insertion preview in the [`insert-hint` layout config](./Configuration:-Layout.md#insert-hint).

### All Pointing Devices

#### Drag-and-drop edge workspace switch

Scroll the workspaces up/down when moving the mouse cursor against a monitor edge during drag-and-drop (DnD) while in the overview.
Also works on a touchscreen.

#### Drag-and-drop hold to activate

While drag-and-dropping, hold your mouse over a window to activate it.
This will bring a floating window to the top for example.

In the overview, you can also hold the mouse over a workspace to switch to it.

#### Hot corner to toggle the overview

Put your mouse at an enabled monitor corner to toggle the overview.
Also works during drag-and-dropping something.
