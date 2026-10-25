//! Dragging a window or a floating container with the pointer, and drag-and-drop over the
//! layout.

use super::*;

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(super) enum InteractiveMoveState<W: LayoutElement> {
    /// Initial rubberbanding; the window remains in the layout.
    Starting {
        /// The window we're moving.
        window_id: W::Id,
        /// Current pointer delta from the starting location.
        pointer_delta: Point<f64, Logical>,
        /// Pointer location within the visual window geometry as ratio from geometry size.
        ///
        /// This helps the pointer remain inside the window as it resizes.
        pointer_ratio_within_window: (f64, f64),
    },
    /// Moving; the window is an in-flight transfer outside every workspace but remains in the
    /// layout tree.
    Moving(InteractiveMoveData<W>),
    /// Moving a floating container; the windows remain in the layout.
    MovingContainer(InteractiveMoveContainerData<W>),
}

#[derive(Debug)]
pub(super) struct InteractiveMoveData<W: LayoutElement> {
    /// View node held by the interactive move while attached to no workspace.
    pub(super) node: NodeKey,
    /// Stable id cached so routing does not need to borrow the in-flight tile.
    pub(super) window_id: W::Id,
    /// Output where the window is currently located/rendered.
    pub(super) output: Output,
    /// Current pointer position within output.
    pub(super) pointer_pos_within_output: Point<f64, Logical>,
    /// Whether the window targets the floating layout.
    pub(super) is_floating: bool,
    /// Pointer location within the visual window geometry as ratio from geometry size.
    ///
    /// This helps the pointer remain inside the window as it resizes.
    pub(super) pointer_ratio_within_window: (f64, f64),
    /// Config overrides for the output where the window is currently located.
    ///
    /// Cached here to be accessible while an output is removed.
    pub(super) output_config: Option<tiri_config::LayoutPart>,
    /// Config overrides for the workspace where the window is currently located.
    ///
    /// To avoid sudden window changes when starting an interactive move, it will remember the
    /// config overrides for the workspace where the move originated from. As soon as the window
    /// moves over some different workspace though, this override will reset.
    pub(super) workspace_config: Option<(WorkspaceId, tiri_config::LayoutPart)>,
    /// Original insert location for swaps within the tiling layout.
    pub(super) swap_origin: Option<InsertParentInfo>,
    /// Workspace where the move originated.
    pub(super) origin_workspace: WorkspaceId,
}

#[derive(Debug)]
pub(super) struct InteractiveMoveContainerData<W: LayoutElement> {
    /// The window being moved (used to find its container).
    pub(super) window_id: W::Id,
    /// Output where the pointer is currently located.
    pub(super) output: Output,
    /// Current pointer position within output.
    pub(super) pointer_pos_within_output: Point<f64, Logical>,
    /// Pointer position at the start of the container move.
    pub(super) start_pointer_pos_within_output: Point<f64, Logical>,
    /// Container position at the start of the move (workspace coords).
    pub(super) start_container_pos: Point<f64, Logical>,
}

#[derive(Debug)]
pub struct DndData<W: LayoutElement> {
    /// Output where the pointer is currently located.
    pub(super) output: Output,
    /// Current pointer position within output.
    pub(super) pointer_pos_within_output: Point<f64, Logical>,
    /// Ongoing DnD hold to activate something.
    pub(super) hold: Option<DndHold<W>>,
}

#[derive(Debug)]
pub(super) struct DndHold<W: LayoutElement> {
    /// Time when we started holding on the target.
    pub(super) start_time: Duration,
    pub(super) target: DndHoldTarget<W::Id>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DndHoldTarget<WindowId> {
    Window(WindowId),
    Workspace(WorkspaceId),
}

impl<W: LayoutElement> InteractiveMoveState<W> {
    pub(super) fn moving(&self) -> Option<&InteractiveMoveData<W>> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(move_),
            _ => None,
        }
    }

    pub(super) fn moving_container(&self) -> Option<&InteractiveMoveContainerData<W>> {
        match self {
            InteractiveMoveState::MovingContainer(move_) => Some(move_),
            _ => None,
        }
    }

    pub(super) fn moving_window_id(&self) -> Option<&W::Id> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(&move_.window_id),
            InteractiveMoveState::MovingContainer(move_) => Some(&move_.window_id),
            _ => None,
        }
    }

    pub(super) fn is_moving(&self) -> bool {
        matches!(
            self,
            InteractiveMoveState::Moving(_) | InteractiveMoveState::MovingContainer(_)
        )
    }
}

impl<W: LayoutElement> InteractiveMoveData<W> {
    pub(super) fn tile_render_location(&self, tile: &Tile<W>, zoom: f64) -> Point<f64, Logical> {
        let scale = Scale::from(self.output.current_scale().fractional_scale());
        let window_size = tile.window_size();
        let pointer_offset_within_window = Point::from((
            window_size.w * self.pointer_ratio_within_window.0,
            window_size.h * self.pointer_ratio_within_window.1,
        ));
        let pos = self.pointer_pos_within_output
            - (pointer_offset_within_window + tile.window_loc() - tile.render_offset())
                .upscale(zoom);
        // Round to physical pixels.
        pos.to_physical_precise_round(scale).to_logical(scale)
    }
}

impl<W: LayoutElement> Layout<W> {
    pub fn interactive_move_begin(
        &mut self,
        window_id: W::Id,
        output: &Output,
        start_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        if self.interactive_move.is_some() {
            return false;
        }

        let found = match self.window_owner(&window_id).map(|(_, owner)| owner) {
            Some(WorkspaceOwner::Monitor {
                monitor_idx,
                workspace_idx,
            }) => {
                let Some(mon) = self.monitors().nth(monitor_idx) else {
                    return false;
                };
                let Some((ws, ws_geo)) = mon
                    .workspaces_with_render_geo(&self.containers)
                    .nth(workspace_idx)
                else {
                    return false;
                };
                let Some((tile, tile_offset, _visible)) = ws
                    .tiles_with_render_positions(&self.containers)
                    .find(|(tile, _, _)| tile.window().id() == &window_id)
                else {
                    return false;
                };
                let window_offset = tile.window_loc();
                let window_size = tile.window_size();
                Some((mon, ws_geo, tile_offset, window_offset, window_size))
            }
            _ => None,
        };

        let Some((mon, ws_geo, tile_offset, window_offset, window_size)) = found else {
            return false;
        };

        if mon.output() != output {
            return false;
        }

        let zoom = mon.overview_zoom();

        let tile_pos = ws_geo.loc + tile_offset.upscale(zoom);

        let pointer_offset_within_window =
            start_pos_within_output - tile_pos - window_offset.upscale(zoom);
        let window_size = window_size.upscale(zoom);
        let pointer_ratio_within_window = (
            f64::clamp(pointer_offset_within_window.x / window_size.w, 0., 1.),
            f64::clamp(pointer_offset_within_window.y / window_size.h, 0., 1.),
        );

        self.interactive_move = Some(InteractiveMoveState::Starting {
            window_id,
            pointer_delta: Point::from((0., 0.)),
            pointer_ratio_within_window,
        });

        for mon in self.monitors_mut() {
            mon.dnd_scroll_gesture_begin();
        }

        true
    }

    /// Set the dragged tile's rubber-band offset. Returns false when the window is gone.
    fn set_interactive_move_offset(&mut self, window: &W::Id, offset: Point<f64, Logical>) -> bool {
        let tree = &mut self.containers;
        let Some(key) = tree.window_key(window) else {
            return false;
        };
        let Some(tile) = tree.get_tile_mut(key) else {
            return false;
        };
        tile.interactive_move_offset = offset;
        true
    }

    /// Whether the window sits in a floating container that is dragged as a whole.
    fn move_container_allows_splits(&self, window: &W::Id) -> bool {
        self.workspace_for_window(window).is_some_and(|workspace| {
            workspace.floating_container_allows_splits(&self.containers, window)
        })
    }

    /// Position of the floating container holding the window.
    fn move_container_pos(&self, window: &W::Id) -> Option<Point<f64, Logical>> {
        self.workspace_for_window(window)
            .and_then(|workspace| workspace.floating_container_pos(&self.containers, window))
    }

    pub fn interactive_move_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
        output: Output,
        pointer_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        let Some(state) = self.interactive_move.take() else {
            return false;
        };

        match state {
            InteractiveMoveState::Starting {
                window_id,
                mut pointer_delta,
                pointer_ratio_within_window,
            } => {
                if window_id != *window {
                    self.interactive_move = Some(InteractiveMoveState::Starting {
                        window_id,
                        pointer_delta,
                        pointer_ratio_within_window,
                    });
                    return false;
                }

                let zoom = self.overview_zoom();
                let delta = delta.downscale(zoom);

                pointer_delta += delta;

                let (cx, cy) = (pointer_delta.x, pointer_delta.y);
                let sq_dist = cx * cx + cy * cy;

                let factor = RubberBand {
                    stiffness: 1.0,
                    limit: 0.5,
                }
                .band(sq_dist / INTERACTIVE_MOVE_START_THRESHOLD);

                let Some(ws) = self.workspace_for_window(&window_id) else {
                    return false;
                };
                let workspace_config = ws
                    .layout_config(&self.containers)
                    .cloned()
                    .map(|c| (ws.id(), c));
                let is_floating = self
                    .containers
                    .window_is_floating_in(ws.workspace_key(), &window_id);
                let floating_grouped = is_floating && self.move_container_allows_splits(&window_id);
                if !self.set_interactive_move_offset(&window_id, pointer_delta.upscale(factor)) {
                    return false;
                }

                // Put it back to be able to easily return.
                self.interactive_move = Some(InteractiveMoveState::Starting {
                    window_id: window_id.clone(),
                    pointer_delta,
                    pointer_ratio_within_window,
                });

                if !is_floating && sq_dist < INTERACTIVE_MOVE_START_THRESHOLD {
                    return true;
                }

                if floating_grouped {
                    // The whole container moves, so the window itself stops rubber-banding.
                    if !self.set_interactive_move_offset(&window_id, Point::from((0., 0.))) {
                        self.interactive_move = None;
                        return false;
                    }
                    // The Starting state was put back above, so bailing out here leaves the
                    // drag exactly as it was.
                    let Some(start_container_pos) = self.move_container_pos(&window_id) else {
                        return false;
                    };

                    self.interactive_move = Some(InteractiveMoveState::MovingContainer(
                        InteractiveMoveContainerData {
                            window_id,
                            output: output.clone(),
                            pointer_pos_within_output,
                            start_pointer_pos_within_output: pointer_pos_within_output,
                            start_container_pos,
                        },
                    ));
                    return true;
                }

                let output_config = self
                    .monitors()
                    .find(|mon| mon.output() == &output)
                    .and_then(|mon| mon.layout_config().cloned());

                // If the pointer is currently on the window's own output, then we can animate the
                // window movement from its current (rubberbanded and possibly moved away) position
                // to the pointer. Otherwise, we just teleport it as the layout code is not aware
                // of monitor positions.
                //
                // FIXME: when and if the layout code knows about monitor positions, this will be
                // potentially animatable.
                let tile_pos = match self.window_owner(window).map(|(_, owner)| owner) {
                    Some(WorkspaceOwner::Monitor {
                        monitor_idx,
                        workspace_idx,
                    }) => self.monitors().nth(monitor_idx).and_then(|mon| {
                        (mon.output() == &output).then_some(mon).and_then(|mon| {
                            let (ws, ws_geo) = mon
                                .workspaces_with_render_geo(&self.containers)
                                .nth(workspace_idx)?;
                            let (_, tile_offset, _) = ws
                                .tiles_with_render_positions(&self.containers)
                                .find(|(tile, _, _)| tile.window().id() == window)?;
                            let zoom = mon.overview_zoom();
                            Some((ws_geo.loc + tile_offset.upscale(zoom), zoom))
                        })
                    }),
                    _ => None,
                };

                // Clear it before calling remove_window() to avoid running interactive_move_end()
                // in the middle of interactive_move_update() and the confusion that causes.
                self.interactive_move = None;

                // Unset fullscreen before removing the tile. This will restore its size properly,
                // and move it to floating if needed, so we don't have to deal with that here.
                let (ws, containers) = self
                    .workspace_for_window_and_containers_mut(&window_id)
                    .unwrap();
                ws.set_fullscreen(containers, window, false);

                let origin_workspace = ws.id();
                let swap_origin = if is_floating {
                    None
                } else {
                    ws.tiling_insert_parent_info(containers, &window_id)
                };
                let transferred = self
                    .take_window_for_transfer(window, Transaction::new())
                    .expect("the interactively moved window must still exist");
                let is_floating = transferred.is_floating;

                transferred
                    .tile_mut(&mut self.containers)
                    .stop_move_animations();
                transferred
                    .tile_mut(&mut self.containers)
                    .interactive_move_offset = Point::from((0., 0.));
                transferred
                    .tile(&self.containers)
                    .window()
                    .output_enter(&output);
                transferred
                    .tile(&self.containers)
                    .window()
                    .set_preferred_scale_transform(
                        output.current_scale(),
                        output.current_transform(),
                    );

                let view_size = output_size(&output);
                let scale = output.current_scale().fractional_scale();
                let options = Options::clone(&self.options)
                    .with_merged_layout(output_config.as_ref())
                    .with_merged_layout(workspace_config.as_ref().map(|(_, c)| c))
                    .adjusted_for_scale(scale);
                transferred.tile_mut(&mut self.containers).update_config(
                    view_size,
                    scale,
                    Rc::new(options),
                );
                if !transferred
                    .tile(&self.containers)
                    .window()
                    .pending_sizing_mode()
                    .is_normal()
                {
                    let tile_size = transferred.tile(&self.containers).tile_size();
                    transferred
                        .tile_mut(&mut self.containers)
                        .request_tile_size(tile_size, !self.options.animations.off, None);
                }

                if !is_floating {
                    // Animate to semitransparent.
                    transferred.tile_mut(&mut self.containers).animate_alpha(
                        1.,
                        INTERACTIVE_MOVE_ALPHA,
                        self.options.animations.window_movement.0,
                    );
                    transferred
                        .tile_mut(&mut self.containers)
                        .hold_alpha_animation_after_done();
                }

                let window_id = transferred.tile(&self.containers).window().id().clone();
                let node = transferred.node;
                let data = InteractiveMoveData {
                    node,
                    window_id,
                    output,
                    pointer_pos_within_output,
                    is_floating,
                    pointer_ratio_within_window,
                    output_config,
                    workspace_config,
                    swap_origin,
                    origin_workspace,
                };

                if let Some((tile_pos, zoom)) = tile_pos {
                    let new_tile_pos = data.tile_render_location(
                        self.containers
                            .transferred_tile(node)
                            .expect("moving view must remain transferred"),
                        zoom,
                    );
                    self.containers
                        .transferred_tile_mut(node)
                        .expect("moving view must remain transferred")
                        .animate_move_from((tile_pos - new_tile_pos).downscale(zoom));
                }

                self.interactive_move = Some(InteractiveMoveState::Moving(data));
            }
            InteractiveMoveState::Moving(mut move_) => {
                if window != &move_.window_id {
                    self.interactive_move = Some(InteractiveMoveState::Moving(move_));
                    return false;
                }

                let mut ws_id = None;
                if let Some(mon) = self.monitor_for_output(&output) {
                    let (insert_ws, _) =
                        mon.insert_position(&self.containers, move_.pointer_pos_within_output);
                    if let InsertWorkspace::Existing(id) = insert_ws {
                        ws_id = Some(id);
                    }
                }

                // If moved over a different workspace, reset the config override.
                let mut update_config = false;
                if let Some((id, _)) = &move_.workspace_config {
                    if Some(*id) != ws_id {
                        move_.workspace_config = None;
                        update_config = true;
                    }
                }

                if output != move_.output {
                    let window = self
                        .containers
                        .transferred_tile(move_.node)
                        .expect("moving view must remain transferred")
                        .window();
                    window.output_leave(&move_.output);
                    window.output_enter(&output);
                    window.set_preferred_scale_transform(
                        output.current_scale(),
                        output.current_transform(),
                    );
                    move_.output = output.clone();
                    self.focus_output(&output);

                    move_.output_config = self
                        .monitor_for_output(&output)
                        .and_then(|mon| mon.layout_config().cloned());

                    update_config = true;
                }

                if update_config {
                    let view_size = output_size(&output);
                    let scale = output.current_scale().fractional_scale();
                    let options = Options::clone(&self.options)
                        .with_merged_layout(move_.output_config.as_ref())
                        .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                        .adjusted_for_scale(scale);
                    self.containers
                        .transferred_tile_mut(move_.node)
                        .expect("moving view must remain transferred")
                        .update_config(view_size, scale, Rc::new(options));
                }

                move_.pointer_pos_within_output = pointer_pos_within_output;

                self.interactive_move = Some(InteractiveMoveState::Moving(move_));
            }
            InteractiveMoveState::MovingContainer(mut move_) => {
                if window != &move_.window_id {
                    self.interactive_move = Some(InteractiveMoveState::MovingContainer(move_));
                    return false;
                }

                if output != move_.output {
                    self.interactive_move = Some(InteractiveMoveState::MovingContainer(move_));
                    return false;
                }

                move_.pointer_pos_within_output = pointer_pos_within_output;

                let zoom = self.overview_zoom();
                let delta = (move_.pointer_pos_within_output
                    - move_.start_pointer_pos_within_output)
                    .downscale(zoom);
                let new_pos = move_.start_container_pos + delta;

                if let Some((ws, containers)) =
                    self.workspace_for_window_and_containers_mut(&move_.window_id)
                {
                    ws.move_floating_container_for_window_to(containers, &move_.window_id, new_pos);
                }

                self.interactive_move = Some(InteractiveMoveState::MovingContainer(move_));
            }
        }

        true
    }

    pub fn interactive_move_end(&mut self, window: &W::Id) {
        self.request_refresh();
        let Some(move_) = &self.interactive_move else {
            return;
        };

        let move_ = match move_ {
            InteractiveMoveState::Starting { window_id, .. } => {
                if window_id != window {
                    return;
                }

                let Some(InteractiveMoveState::Starting { window_id, .. }) =
                    self.interactive_move.take()
                else {
                    unreachable!()
                };

                self.end_dnd_scroll_gestures();

                if let Some((ws, containers)) =
                    self.workspace_for_window_and_containers_mut(&window_id)
                {
                    if let Some(tile) = containers
                        .tiles_mut_in_workspace(ws.workspace_key())
                        .into_iter()
                        .find(|tile| *tile.window().id() == window_id)
                    {
                        let offset = tile.interactive_move_offset;
                        tile.interactive_move_offset = Point::from((0., 0.));
                        tile.animate_move_from(offset);
                    }

                    // Unlock the view on the workspaces, but if the moved window was active,
                    // preserve that.
                    let moved_tile_was_active = ws
                        .active_window(containers)
                        .is_some_and(|win| *win.id() == window_id);

                    if moved_tile_was_active {
                        ws.activate_window(containers, &window_id);
                    }
                }

                return;
            }
            InteractiveMoveState::Moving(move_) => move_,
            InteractiveMoveState::MovingContainer(move_) => {
                if window != &move_.window_id {
                    return;
                }

                let Some(InteractiveMoveState::MovingContainer(move_)) =
                    self.interactive_move.take()
                else {
                    unreachable!()
                };

                self.end_dnd_scroll_gestures();

                if let Some((ws, containers)) =
                    self.workspace_for_window_and_containers_mut(&move_.window_id)
                {
                    if let Some(tile) = containers
                        .tiles_mut_in_workspace(ws.workspace_key())
                        .into_iter()
                        .find(|tile| *tile.window().id() == move_.window_id)
                    {
                        tile.interactive_move_offset = Point::from((0., 0.));
                    }
                }

                return;
            }
        };

        if window != &move_.window_id {
            return;
        }

        let Some(InteractiveMoveState::Moving(move_)) = self.interactive_move.take() else {
            unreachable!()
        };

        self.end_dnd_scroll_gestures();

        let transferred = ViewTransfer {
            node: move_.node,
            is_floating: move_.is_floating,
        };

        // Animate the tile back to opaque.
        if !move_.is_floating {
            transferred.tile_mut(&mut self.containers).animate_alpha(
                INTERACTIVE_MOVE_ALPHA,
                1.,
                self.options.animations.window_movement.0,
            );
        }

        // Dragging in the overview shouldn't switch the workspace and so on.
        let allow_to_activate_workspace = !self.overview_open;

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let (mon, insert_ws, position, offset, zoom) = if let Some(mon) =
                    monitors.iter_mut().find(|mon| mon.output == move_.output)
                {
                    let zoom = mon.overview_zoom();

                    let (insert_ws, geo) =
                        mon.insert_position(&self.containers, move_.pointer_pos_within_output);
                    let (position, offset) = match insert_ws {
                        InsertWorkspace::Existing(ws_id) => {
                            let ws_idx = mon.idx_of_ws(&self.containers, ws_id).unwrap();

                            let position = if move_.is_floating {
                                InsertPosition::Floating
                            } else {
                                let pos_within_workspace =
                                    (move_.pointer_pos_within_output - geo.loc).downscale(zoom);
                                let ws = mon.ws(&self.containers, ws_idx);
                                ws.tiling_insert_position(&self.containers, pos_within_workspace)
                            };

                            (position, Some(geo.loc))
                        }
                        InsertWorkspace::NewAt(_) => {
                            let position = if move_.is_floating {
                                InsertPosition::Floating
                            } else {
                                InsertPosition::RootStart
                            };

                            (position, None)
                        }
                    };

                    (mon, insert_ws, position, offset, zoom)
                } else {
                    let mon = &mut monitors[*active_monitor_idx];
                    let zoom = mon.overview_zoom();
                    // No point in trying to use the pointer position on the wrong output.
                    let ws = mon.ws(&self.containers, 0);
                    let ws_geo = mon.workspaces_render_geo(&self.containers).next().unwrap();

                    let position = if move_.is_floating {
                        InsertPosition::Floating
                    } else {
                        ws.tiling_insert_position(&self.containers, Point::from((0., 0.)))
                    };

                    let insert_ws = InsertWorkspace::Existing(ws.id());
                    (mon, insert_ws, position, Some(ws_geo.loc), zoom)
                };

                let win_id = move_.window_id.clone();
                let tile_render_loc =
                    move_.tile_render_location(transferred.tile(&self.containers), zoom);

                let ws_idx = match insert_ws {
                    InsertWorkspace::Existing(ws_id) => {
                        mon.idx_of_ws(&self.containers, ws_id).unwrap()
                    }
                    InsertWorkspace::NewAt(ws_idx) => {
                        if mon.options.layout.empty_workspace_above_first && ws_idx == 0 {
                            // Reuse the top empty workspace.
                            0
                        } else if mon.workspace_count(&self.containers) - 1 <= ws_idx {
                            // Reuse the bottom empty workspace.
                            mon.workspace_count(&self.containers) - 1
                        } else {
                            mon.add_workspace_at(&mut self.containers, ws_idx);
                            ws_idx
                        }
                    }
                };

                match position {
                    InsertPosition::RootStart => {
                        let ws_id = mon.ws(&self.containers, ws_idx).id();
                        mon.add_tile(
                            &mut self.containers,
                            transferred.into_incoming(),
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                root_index: Some(0),
                            },
                            ActivateWindow::Yes,
                            allow_to_activate_workspace,
                            false,
                        );
                    }
                    InsertPosition::Swap { target, direction } => {
                        // Swapping is only possible back into the workspace the drag started
                        // from, where the vacated slot is still known.
                        let ws_id = mon.ws(&self.containers, ws_idx).id();
                        let origin = (move_.origin_workspace == ws_id)
                            .then(|| move_.swap_origin.clone())
                            .flatten();

                        let tile = transferred.into_incoming();
                        let swap = match origin {
                            Some(origin) => mon.ws(&self.containers, ws_idx).tiling_swap_tile(
                                &mut self.containers,
                                target,
                                tile,
                                &origin,
                            ),
                            None => Err(tile),
                        };

                        match swap {
                            Ok(()) => {
                                if allow_to_activate_workspace {
                                    mon.ws(&self.containers, ws_idx)
                                        .activate_window(&mut self.containers, &win_id);
                                }
                            }
                            Err(tile) => {
                                if let Err(tile) = mon.add_tile_split(
                                    &mut self.containers,
                                    ws_idx,
                                    target,
                                    direction,
                                    tile,
                                    true,
                                    allow_to_activate_workspace,
                                ) {
                                    mon.add_tile(
                                        &mut self.containers,
                                        tile,
                                        MonitorAddWindowTarget::Workspace {
                                            id: ws_id,
                                            root_index: None,
                                        },
                                        ActivateWindow::Yes,
                                        allow_to_activate_workspace,
                                        false,
                                    );
                                }
                            }
                        }
                    }
                    InsertPosition::Split {
                        target, direction, ..
                    } => {
                        let ws_id = mon.ws(&self.containers, ws_idx).id();
                        if let Err(tile) = mon.add_tile_split(
                            &mut self.containers,
                            ws_idx,
                            target,
                            direction,
                            transferred.into_incoming(),
                            true,
                            allow_to_activate_workspace,
                        ) {
                            mon.add_tile(
                                &mut self.containers,
                                tile,
                                MonitorAddWindowTarget::Workspace {
                                    id: ws_id,
                                    root_index: None,
                                },
                                ActivateWindow::Yes,
                                allow_to_activate_workspace,
                                false,
                            );
                        }
                    }
                    InsertPosition::SplitRoot { direction, .. } => {
                        let ws_id = mon.ws(&self.containers, ws_idx).id();
                        if let Err(tile) = mon.add_tile_split_root(
                            &mut self.containers,
                            ws_idx,
                            direction,
                            transferred.into_incoming(),
                            true,
                            allow_to_activate_workspace,
                        ) {
                            mon.add_tile(
                                &mut self.containers,
                                tile,
                                MonitorAddWindowTarget::Workspace {
                                    id: ws_id,
                                    root_index: None,
                                },
                                ActivateWindow::Yes,
                                allow_to_activate_workspace,
                                false,
                            );
                        }
                    }
                    InsertPosition::Floating => {
                        transferred.tile_mut(&mut self.containers).floating_pos = None;

                        match insert_ws {
                            InsertWorkspace::Existing(_) => {
                                if let Some(offset) = offset {
                                    let pos = (tile_render_loc - offset).downscale(zoom);
                                    let pos = mon
                                        .ws(&self.containers, ws_idx)
                                        .floating_logical_to_size_frac(&self.containers, pos);
                                    transferred.tile_mut(&mut self.containers).floating_pos =
                                        Some(pos);
                                } else {
                                    error!(
                                        "offset unset for inserting a floating tile \
                                         to existing workspace"
                                    );
                                }
                            }
                            InsertWorkspace::NewAt(_) => {
                                // When putting a floating tile on a new workspace, we don't really
                                // have a good pre-existing position.
                            }
                        }

                        // Set the floating size so it takes into account any window resizing that
                        // took place during the move.
                        if let Some(size) =
                            transferred.tile(&self.containers).window().expected_size()
                        {
                            transferred
                                .tile_mut(&mut self.containers)
                                .floating_window_size = Some(size);
                        }

                        let ws_id = mon.ws(&self.containers, ws_idx).id();
                        mon.add_tile(
                            &mut self.containers,
                            transferred.into_incoming(),
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                root_index: None,
                            },
                            ActivateWindow::Yes,
                            allow_to_activate_workspace,
                            true,
                        );
                    }
                }

                // needed because empty_workspace_above_first could have modified the idx
                let inserted_ws_idx = mon.workspace_idx_for_window(&self.containers, &win_id);
                if let Some(inserted_ws_idx) = inserted_ws_idx {
                    let ws_geo = mon
                        .workspaces_render_geo(&self.containers)
                        .nth(inserted_ws_idx)
                        .expect("workspace geometry missing after interactive insertion");
                    // No render position when the insert's arrange is still waiting on the
                    // window's configure, the same reason `old_render_pos` above is an option:
                    // a tile that has not been laid out has no place to animate to yet.
                    let workspace = mon.ws(&self.containers, inserted_ws_idx).workspace_key();
                    let landed = self
                        .containers
                        .workspace_tiles_with_render_positions_mut(workspace, false)
                        .find(|(tile, _)| tile.window().id() == &win_id);
                    if let Some((tile, tile_offset)) = landed {
                        let new_tile_render_loc = ws_geo.loc + tile_offset.upscale(zoom);
                        tile.animate_move_from(
                            (tile_render_loc - new_tile_render_loc).downscale(zoom),
                        );
                    }
                }
            }
            MonitorSet::NoOutputs => {
                let ws = self.first_outputless_workspace();

                // No point in trying to use the pointer position without outputs.
                ws.add_tile(
                    &mut self.containers,
                    transferred.into_incoming(),
                    WorkspaceAddWindowTarget::Auto,
                    ActivateWindow::Yes,
                    move_.is_floating,
                );
            }
        }
    }

    pub fn interactive_move_is_moving_above_output(&self, output: &Output) -> bool {
        if let Some(move_) = self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving)
        {
            return move_.output == *output;
        }

        if let Some(move_) = self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_container)
        {
            return move_.output == *output;
        }

        false
    }

    pub fn dnd_update(&mut self, output: Output, pointer_pos_within_output: Point<f64, Logical>) {
        let begin_gesture = self.dnd.is_none();

        self.dnd = Some(DndData {
            output,
            pointer_pos_within_output,
            hold: None,
        });

        if begin_gesture {
            for mon in self.monitors_mut() {
                mon.dnd_scroll_gesture_begin();
            }
        }
    }

    pub fn dnd_end(&mut self) {
        if self.dnd.is_none() {
            return;
        }

        self.dnd = None;

        self.end_dnd_scroll_gestures();
    }
}
