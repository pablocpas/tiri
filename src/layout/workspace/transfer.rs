//! Resolved changes between the tiled and floating branches of one workspace tree.

use crate::layout::container::InactiveTilingReference;

/// A resolved tiling-to-floating mutation.
///
/// Resolution names the semantic thing sway moves. Execution is the only layer that touches
/// the shared tree, floating stack, or focus caches.
pub(super) enum FloatTransfer<I> {
    Workspace,
    SelectedContainer,
    Window { id: I, target_is_active: bool },
}

pub(super) enum UnfloatTransfer<I> {
    Container {
        id: I,
        target_is_active: bool,
        tiling_reference: Option<InactiveTilingReference>,
        was_workspace: bool,
        tiling_was_empty: bool,
    },
    Window {
        id: I,
        target_is_active: bool,
        tiling_reference: Option<InactiveTilingReference>,
    },
}

pub(super) enum FloatingTransfer<I> {
    Float(FloatTransfer<I>),
    Unfloat(UnfloatTransfer<I>),
}

impl<I> FloatingTransfer<I> {
    pub(super) fn animation_window_id(&self) -> Option<&I> {
        match self {
            Self::Float(FloatTransfer::Window { id, .. })
            | Self::Unfloat(UnfloatTransfer::Container { id, .. })
            | Self::Unfloat(UnfloatTransfer::Window { id, .. }) => Some(id),
            Self::Float(FloatTransfer::Workspace | FloatTransfer::SelectedContainer) => None,
        }
    }
}
