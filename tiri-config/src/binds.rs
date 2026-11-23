use std::collections::HashSet;
use std::str::FromStr;
use std::time::Duration;

use bitflags::bitflags;
use knuffel::errors::DecodeError;
use miette::miette;
use smithay::input::keyboard::keysyms::KEY_NoSymbol;
use smithay::input::keyboard::xkb::{keysym_from_name, KEYSYM_CASE_INSENSITIVE, KEYSYM_NO_FLAGS};
use smithay::input::keyboard::Keysym;
use tiri_ipc::{
    ColumnDisplay, LayoutCycleTarget, LayoutSwitchTarget, PositionChange, SizeChange,
    WorkspaceReferenceArg,
};
use tracing::warn;

use crate::input::ModKey;
use crate::recent_windows::{MruDirection, MruFilter, MruScope};
use crate::utils::{expect_only_children, MergeWith};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Binds(pub Vec<Bind>);

#[derive(Debug, Clone, PartialEq)]
pub struct ModeBinds {
    pub name: String,
    pub binds: Binds,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bind {
    pub key: Key,
    pub action: BoundAction,
    pub repeat: bool,
    pub pass_through: Option<bool>,
    pub cooldown: Option<Duration>,
    pub allow_when_locked: bool,
    pub allow_inhibiting: bool,
    pub hotkey_overlay_title: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundAction {
    Press(Action),
    Release(Action),
    Both { press: Action, release: Action },
}

impl Bind {
    pub fn has_press(&self) -> bool {
        matches!(
            self.action,
            BoundAction::Press(_) | BoundAction::Both { .. }
        )
    }

    pub fn has_release(&self) -> bool {
        matches!(
            self.action,
            BoundAction::Release(_) | BoundAction::Both { .. }
        )
    }

    pub fn press_action(&self) -> Option<&Action> {
        match &self.action {
            BoundAction::Press(action) => Some(action),
            BoundAction::Both { press, .. } => Some(press),
            BoundAction::Release(_) => None,
        }
    }

    pub fn release_action(&self) -> Option<&Action> {
        match &self.action {
            BoundAction::Release(action) => Some(action),
            BoundAction::Both { release, .. } => Some(release),
            BoundAction::Press(_) => None,
        }
    }

    pub fn into_action(self, pressed: bool) -> Option<Action> {
        match (self.action, pressed) {
            (BoundAction::Press(action), true) | (BoundAction::Release(action), false) => {
                Some(action)
            }
            (BoundAction::Both { press, .. }, true) => Some(press),
            (BoundAction::Both { release, .. }, false) => Some(release),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct Key {
    pub trigger: Trigger,
    pub modifiers: Modifiers,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Trigger {
    Keysym(Keysym),
    CompositorMod,
    Modifier(ModKey),
    MouseLeft,
    MouseRight,
    MouseMiddle,
    MouseBack,
    MouseForward,
    WheelScrollDown,
    WheelScrollUp,
    WheelScrollLeft,
    WheelScrollRight,
    TouchpadScrollDown,
    TouchpadScrollUp,
    TouchpadScrollLeft,
    TouchpadScrollRight,
    TabletStylusButton1,
    TabletStylusButton2,
    TabletStylusButton3,
}

/// Returns whether `keysym` is a modifier key like `Shift_L`.
///
/// This function matches what the niri config considers modifiers.
/// So, it does not correspond exactly to `keysym.is_modifier_key()`.
/// For example, unlike XKB, `Caps_Lock` and `ISO_Next_Group` aren't considered modifiers.
pub fn is_keysym_modifier(keysym: Keysym) -> bool {
    ModKey::from_keysym(keysym).is_some()
}

impl Trigger {
    pub fn is_modifier(self) -> bool {
        match self {
            Self::CompositorMod | Self::Modifier(_) => true,
            Self::Keysym(keysym) => is_keysym_modifier(keysym),
            _ => false,
        }
    }

    pub fn is_generic_modifier(self) -> bool {
        matches!(self, Self::CompositorMod | Self::Modifier(_))
    }

    /// The modifier flags that this trigger sets, if it is a modifier key.
    pub fn implied_modifier(self) -> Modifiers {
        match self {
            Self::CompositorMod => Modifiers::COMPOSITOR,
            Self::Modifier(modifier) => modifier.to_modifiers(),
            Self::Keysym(keysym) => ModKey::from_keysym(keysym)
                .map_or(Modifiers::empty(), |modifier| modifier.to_modifiers()),
            _ => Modifiers::empty(),
        }
    }

    pub fn is_keyboard(self) -> bool {
        matches!(
            self,
            Trigger::Keysym(_) | Trigger::CompositorMod | Trigger::Modifier(_)
        )
    }

    pub fn is_scroll(self) -> bool {
        matches!(
            self,
            Trigger::WheelScrollDown
                | Trigger::WheelScrollUp
                | Trigger::WheelScrollLeft
                | Trigger::WheelScrollRight
                | Trigger::TouchpadScrollDown
                | Trigger::TouchpadScrollUp
                | Trigger::TouchpadScrollLeft
                | Trigger::TouchpadScrollRight
        )
    }
}

impl Key {
    /// Whether the two keys are triggered by the same key press.
    ///
    /// Two different keybinds can be triggered by the same key press. For example, a modifier key
    /// can be spelled as the modifier itself (`Alt`) or as its keysym (`Alt_L`).
    pub fn conflicts_with(&self, other: &Self) -> bool {
        // The trigger and the modifiers match exactly.
        //
        // All non-modifier triggers fall into this category.
        if *self == *other {
            return true;
        }

        // Modifiers include the trigger's implied modifier, so differing modifiers can never be
        // triggered by the same key press.
        if self.modifiers != other.modifiers {
            return false;
        }

        // Generic-modifier-only binds are press-order-independent, so only their modifier set
        // (which already includes the trigger implied modifier) has to match.
        (self.trigger.is_generic_modifier() && other.trigger.is_modifier())
            || (self.trigger.is_modifier() && other.trigger.is_generic_modifier())
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Modifiers : u8 {
        const CTRL = 1;
        const SHIFT = 1 << 1;
        const ALT = 1 << 2;
        const SUPER = 1 << 3;
        const ISO_LEVEL3_SHIFT = 1 << 4;
        const ISO_LEVEL5_SHIFT = 1 << 5;
        const COMPOSITOR = 1 << 6;
    }
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct SwitchBinds {
    #[knuffel(child)]
    pub lid_open: Option<SwitchAction>,
    #[knuffel(child)]
    pub lid_close: Option<SwitchAction>,
    #[knuffel(child)]
    pub tablet_mode_on: Option<SwitchAction>,
    #[knuffel(child)]
    pub tablet_mode_off: Option<SwitchAction>,
}

impl MergeWith<SwitchBinds> for SwitchBinds {
    fn merge_with(&mut self, part: &SwitchBinds) {
        merge_clone_opt!(
            (self, part),
            lid_open,
            lid_close,
            tablet_mode_on,
            tablet_mode_off,
        );
    }
}

#[derive(knuffel::Decode, Debug, Clone, PartialEq)]
pub struct SwitchAction {
    #[knuffel(child, unwrap(arguments))]
    pub spawn: Vec<String>,
}

// Remember to add new actions to the CLI enum too.
#[derive(knuffel::Decode, Debug, Clone, PartialEq)]
pub enum Action {
    Quit(#[knuffel(property(name = "skip-confirmation"), default)] bool),
    ChangeVt(#[knuffel(argument)] u8),
    Suspend,
    #[knuffel(skip)]
    PowerKeySuspend,
    PowerOffMonitors,
    PowerOnMonitors,
    ToggleDebugTint,
    DebugToggleOpaqueRegions,
    DebugToggleDamage,
    ToggleTouchpad,
    Spawn(#[knuffel(arguments)] Vec<String>),
    SpawnSh(#[knuffel(argument)] String),
    DoScreenTransition(#[knuffel(property(name = "delay-ms"))] Option<u16>),
    #[knuffel(skip)]
    ConfirmScreenshot {
        write_to_disk: bool,
    },
    #[knuffel(skip)]
    CancelScreenshot,
    #[knuffel(skip)]
    ScreenshotTogglePointer,
    Screenshot(
        #[knuffel(property(name = "show-pointer"), default = true)] bool,
        // Path; not settable from knuffel
        Option<String>,
    ),
    ScreenshotScreen(
        #[knuffel(property(name = "write-to-disk"), default = true)] bool,
        #[knuffel(property(name = "show-pointer"), default = true)] bool,
        // Path; not settable from knuffel
        Option<String>,
    ),
    ScreenshotWindow(
        #[knuffel(property(name = "write-to-disk"), default = true)] bool,
        #[knuffel(property(name = "show-pointer"), default = false)] bool,
        // Path; not settable from knuffel
        Option<String>,
    ),
    #[knuffel(skip)]
    ScreenshotWindowById {
        id: u64,
        write_to_disk: bool,
        show_pointer: bool,
        path: Option<String>,
    },
    ToggleKeyboardShortcutsInhibit,
    CloseWindow,
    #[knuffel(skip)]
    CloseWindowById(u64),
    FullscreenWindow,
    #[knuffel(skip)]
    FullscreenWindowById(u64),
    ToggleWindowedFullscreen,
    #[knuffel(skip)]
    ToggleWindowedFullscreenById(u64),
    MoveWindowToScratchpad,
    #[knuffel(skip)]
    MoveWindowToScratchpadById(u64),
    ScratchpadShow,
    Mark(#[knuffel(argument)] String),
    MarkAdd(#[knuffel(argument)] String),
    MarkToggle(#[knuffel(argument)] String),
    MarkReplace(#[knuffel(argument)] String),
    Unmark(#[knuffel(argument)] Option<String>),
    SetWindowFullscreen,
    #[knuffel(skip)]
    SetWindowFullscreenById(u64),
    UnsetWindowFullscreen,
    #[knuffel(skip)]
    UnsetWindowFullscreenById(u64),
    SetWindowMaximizedToEdges,
    #[knuffel(skip)]
    SetWindowMaximizedToEdgesById(u64),
    UnsetWindowMaximizedToEdges,
    #[knuffel(skip)]
    UnsetWindowMaximizedToEdgesById(u64),
    SetWindowedFullscreen,
    #[knuffel(skip)]
    SetWindowedFullscreenById(u64),
    UnsetWindowedFullscreen,
    #[knuffel(skip)]
    UnsetWindowedFullscreenById(u64),
    #[knuffel(skip)]
    FocusWindow(u64),
    FocusWindowInColumn(#[knuffel(argument)] u8),
    FocusWindowPrevious(
        #[knuffel(property(name = "scope"), default)] MruScope,
        #[knuffel(property(name = "filter"), default)] MruFilter,
    ),
    FocusColumnLeft,
    #[knuffel(skip)]
    FocusColumnLeftUnderMouse,
    FocusColumnRight,
    #[knuffel(skip)]
    FocusColumnRightUnderMouse,
    FocusContainerLeft,
    FocusContainerDown,
    FocusContainerUp,
    FocusContainerRight,
    FocusColumnFirst,
    FocusColumnLast,
    FocusColumnRightOrFirst,
    FocusColumnLeftOrLast,
    FocusColumn(#[knuffel(argument)] usize),
    FocusWindowOrMonitorUp,
    FocusWindowOrMonitorDown,
    FocusColumnOrMonitorLeft,
    FocusColumnOrMonitorRight,
    FocusWindowDown,
    FocusWindowUp,
    FocusWindowDownOrColumnLeft,
    FocusWindowDownOrColumnRight,
    FocusWindowUpOrColumnLeft,
    FocusWindowUpOrColumnRight,
    FocusWindowOrWorkspaceDown,
    FocusWindowOrWorkspaceUp,
    FocusWindowTop,
    FocusWindowBottom,
    FocusWindowDownOrTop,
    FocusWindowUpOrBottom,
    MoveColumnLeft,
    MoveColumnRight,
    MoveColumnToFirst,
    MoveColumnToLast,
    MoveColumnLeftOrToMonitorLeft,
    MoveColumnRightOrToMonitorRight,
    MoveColumnToIndex(#[knuffel(argument)] usize),
    MoveContainerLeft,
    MoveContainerDown,
    MoveContainerUp,
    MoveContainerRight,
    MoveContainerToFirst,
    MoveContainerToLast,
    MoveContainerLeftOrToMonitorLeft,
    MoveContainerRightOrToMonitorRight,
    MoveContainerToIndex(#[knuffel(argument)] usize),
    MoveWindowDown,
    MoveWindowUp,
    MoveWindowDownOrToWorkspaceDown,
    MoveWindowUpOrToWorkspaceUp,
    ConsumeOrExpelWindowLeft,
    #[knuffel(skip)]
    ConsumeOrExpelWindowLeftById(u64),
    ConsumeOrExpelWindowRight,
    #[knuffel(skip)]
    ConsumeOrExpelWindowRightById(u64),
    ConsumeWindowIntoColumn,
    ExpelWindowFromColumn,
    ConsumeWindowIntoContainer,
    ExpelWindowFromContainer,
    SwapWindowWithMark(#[knuffel(argument)] String),
    #[knuffel(skip)]
    SwapWindowWithId(u64),
    ToggleColumnTabbedDisplay,
    SetColumnDisplay(#[knuffel(argument, str)] ColumnDisplay),
    CenterWindow,
    #[knuffel(skip)]
    CenterWindowById(u64),
    FocusWorkspaceDown,
    #[knuffel(skip)]
    FocusWorkspaceDownUnderMouse,
    FocusWorkspaceUp,
    #[knuffel(skip)]
    FocusWorkspaceUpUnderMouse,
    FocusWorkspace(#[knuffel(argument)] WorkspaceReference),
    FocusWorkspacePrevious,
    MoveWindowToWorkspaceDown(#[knuffel(property(name = "focus"), default = true)] bool),
    MoveWindowToWorkspaceUp(#[knuffel(property(name = "focus"), default = true)] bool),
    MoveWindowToWorkspace(
        #[knuffel(argument)] WorkspaceReference,
        #[knuffel(property(name = "focus"), default = true)] bool,
    ),
    #[knuffel(skip)]
    MoveWindowToWorkspaceById {
        window_id: u64,
        reference: WorkspaceReference,
        focus: bool,
    },
    MoveColumnToWorkspaceDown(#[knuffel(property(name = "focus"), default = true)] bool),
    MoveColumnToWorkspaceUp(#[knuffel(property(name = "focus"), default = true)] bool),
    MoveColumnToWorkspace(
        #[knuffel(argument)] WorkspaceReference,
        #[knuffel(property(name = "focus"), default = true)] bool,
    ),
    MoveContainerToWorkspaceDown(#[knuffel(property(name = "focus"), default = true)] bool),
    MoveContainerToWorkspaceUp(#[knuffel(property(name = "focus"), default = true)] bool),
    MoveContainerToWorkspace(
        #[knuffel(argument)] WorkspaceReference,
        #[knuffel(property(name = "focus"), default = true)] bool,
    ),
    MoveWorkspaceDown,
    MoveWorkspaceUp,
    MoveWorkspaceToIndex(#[knuffel(argument)] usize),
    #[knuffel(skip)]
    MoveWorkspaceToIndexByRef {
        new_idx: usize,
        reference: WorkspaceReference,
    },
    #[knuffel(skip)]
    MoveWorkspaceToMonitorByRef {
        output_name: String,
        reference: WorkspaceReference,
    },
    MoveWorkspaceToMonitor(#[knuffel(argument)] String),
    SetWorkspaceName(#[knuffel(argument)] String),
    #[knuffel(skip)]
    SetWorkspaceNameByRef {
        name: String,
        reference: WorkspaceReference,
    },
    UnsetWorkspaceName,
    #[knuffel(skip)]
    UnsetWorkSpaceNameByRef(#[knuffel(argument)] WorkspaceReference),
    FocusMonitorLeft,
    FocusMonitorRight,
    FocusMonitorDown,
    FocusMonitorUp,
    FocusMonitorPrevious,
    FocusMonitorNext,
    FocusMonitor(#[knuffel(argument)] String),
    MoveWindowToMonitorLeft,
    MoveWindowToMonitorRight,
    MoveWindowToMonitorDown,
    MoveWindowToMonitorUp,
    MoveWindowToMonitorPrevious,
    MoveWindowToMonitorNext,
    MoveWindowToMonitor(#[knuffel(argument)] String),
    #[knuffel(skip)]
    MoveWindowToMonitorById {
        id: u64,
        output: String,
    },
    MoveColumnToMonitorLeft,
    MoveColumnToMonitorRight,
    MoveColumnToMonitorDown,
    MoveColumnToMonitorUp,
    MoveColumnToMonitorPrevious,
    MoveColumnToMonitorNext,
    MoveColumnToMonitor(#[knuffel(argument)] String),
    MoveContainerToMonitorLeft,
    MoveContainerToMonitorRight,
    MoveContainerToMonitorDown,
    MoveContainerToMonitorUp,
    MoveContainerToMonitorPrevious,
    MoveContainerToMonitorNext,
    MoveContainerToMonitor(#[knuffel(argument)] String),
    ResizeGrowWidth,
    ResizeShrinkWidth,
    ResizeGrowHeight,
    ResizeShrinkHeight,
    ResizeGrowLeft,
    ResizeShrinkLeft,
    ResizeGrowRight,
    ResizeShrinkRight,
    ResizeGrowUp,
    ResizeShrinkUp,
    ResizeGrowDown,
    ResizeShrinkDown,
    FocusParent,
    FocusChild,
    SplitHorizontal,
    SplitVertical,
    SplitToggle,
    SplitNone,
    ToggleAutotile,
    SetLayoutSplitH,
    SetLayoutSplitV,
    ToggleSplitLayout,
    SetLayoutDefault,
    ToggleLayout(#[knuffel(arguments, str)] Vec<LayoutCycleTarget>),
    ToggleLayoutAll,
    SetLayoutStacked,
    SetLayoutTabbed,
    SetWindowWidth(#[knuffel(argument, str)] SizeChange),
    #[knuffel(skip)]
    SetWindowWidthById {
        id: u64,
        change: SizeChange,
    },
    SetWindowHeight(#[knuffel(argument, str)] SizeChange),
    #[knuffel(skip)]
    SetWindowHeightById {
        id: u64,
        change: SizeChange,
    },
    ResetWindowHeight,
    #[knuffel(skip)]
    ResetWindowHeightById(u64),
    SwitchPresetColumnWidth,
    SwitchPresetColumnWidthBack,
    SwitchPresetWindowWidth,
    SwitchPresetWindowWidthBack,
    #[knuffel(skip)]
    SwitchPresetWindowWidthById(u64),
    #[knuffel(skip)]
    SwitchPresetWindowWidthBackById(u64),
    SwitchPresetWindowHeight,
    SwitchPresetWindowHeightBack,
    #[knuffel(skip)]
    SwitchPresetWindowHeightById(u64),
    #[knuffel(skip)]
    SwitchPresetWindowHeightBackById(u64),
    SetColumnWidth(#[knuffel(argument, str)] SizeChange),
    ExpandColumnToAvailableWidth,
    SwitchLayout(#[knuffel(argument, str)] LayoutSwitchTarget),
    Mode(#[knuffel(argument)] String),
    ShowHotkeyOverlay,
    MoveWorkspaceToMonitorLeft,
    MoveWorkspaceToMonitorRight,
    MoveWorkspaceToMonitorDown,
    MoveWorkspaceToMonitorUp,
    MoveWorkspaceToMonitorPrevious,
    MoveWorkspaceToMonitorNext,
    ToggleWindowFloating,
    #[knuffel(skip)]
    ToggleWindowFloatingById(u64),
    ToggleWindowSticky,
    #[knuffel(skip)]
    ToggleWindowStickyById(u64),
    MoveWindowToFloating,
    #[knuffel(skip)]
    MoveWindowToFloatingById(u64),
    MoveWindowToTiling,
    #[knuffel(skip)]
    MoveWindowToTilingById(u64),
    FocusFloating,
    FocusTiling,
    SwitchFocusBetweenFloatingAndTiling,
    #[knuffel(skip)]
    MoveFloatingWindowById {
        id: Option<u64>,
        x: PositionChange,
        y: PositionChange,
    },
    ToggleWindowRuleOpacity,
    #[knuffel(skip)]
    ToggleWindowRuleOpacityById(u64),
    SetDynamicCastWindow,
    #[knuffel(skip)]
    SetDynamicCastWindowById(u64),
    SetDynamicCastMonitor(#[knuffel(argument)] Option<String>),
    ClearDynamicCastTarget,
    #[knuffel(skip)]
    StopCast(u64),
    ToggleOverview,
    OpenOverview,
    CloseOverview,
    #[knuffel(skip)]
    ToggleWindowUrgent(u64),
    #[knuffel(skip)]
    SetWindowUrgent(u64),
    #[knuffel(skip)]
    UnsetWindowUrgent(u64),
    #[knuffel(skip)]
    LoadConfigFile(#[knuffel(argument)] Option<String>),
    #[knuffel(skip)]
    MruAdvance {
        direction: MruDirection,
        scope: Option<MruScope>,
        filter: Option<MruFilter>,
    },
    #[knuffel(skip)]
    MruConfirm,
    #[knuffel(skip)]
    MruCancel,
    #[knuffel(skip)]
    MruCloseCurrentWindow,
    #[knuffel(skip)]
    MruFirst,
    #[knuffel(skip)]
    MruLast,
    #[knuffel(skip)]
    MruSetScope(MruScope),
    #[knuffel(skip)]
    MruCycleScope,
    #[knuffel(skip)]
    GlobalShortcutActivateAction(u32),
    #[knuffel(skip)]
    GlobalShortcutDeactivateAction(u32),

    // Doesn't look like knuffel allows gating on #[cfg(test)]...
    #[knuffel(skip)]
    TestAction,
}

impl From<tiri_ipc::Action> for Action {
    fn from(value: tiri_ipc::Action) -> Self {
        match value {
            tiri_ipc::Action::Quit { skip_confirmation } => Self::Quit(skip_confirmation),
            tiri_ipc::Action::ChangeVt { vt } => Self::ChangeVt(vt),
            tiri_ipc::Action::PowerOffMonitors {} => Self::PowerOffMonitors,
            tiri_ipc::Action::PowerOnMonitors {} => Self::PowerOnMonitors,
            tiri_ipc::Action::Spawn { command } => Self::Spawn(command),
            tiri_ipc::Action::SpawnSh { command } => Self::SpawnSh(command),
            tiri_ipc::Action::DoScreenTransition { delay_ms } => Self::DoScreenTransition(delay_ms),
            tiri_ipc::Action::Screenshot { show_pointer, path } => {
                Self::Screenshot(show_pointer, path)
            }
            tiri_ipc::Action::ScreenshotScreen {
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotScreen(write_to_disk, show_pointer, path),
            tiri_ipc::Action::ScreenshotWindow {
                id: None,
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotWindow(write_to_disk, show_pointer, path),
            tiri_ipc::Action::ScreenshotWindow {
                id: Some(id),
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotWindowById {
                id,
                write_to_disk,
                show_pointer,
                path,
            },
            tiri_ipc::Action::ToggleKeyboardShortcutsInhibit {} => {
                Self::ToggleKeyboardShortcutsInhibit
            }
            tiri_ipc::Action::CloseWindow { id: None } => Self::CloseWindow,
            tiri_ipc::Action::CloseWindow { id: Some(id) } => Self::CloseWindowById(id),
            tiri_ipc::Action::FullscreenWindow { id: None } => Self::FullscreenWindow,
            tiri_ipc::Action::FullscreenWindow { id: Some(id) } => Self::FullscreenWindowById(id),
            tiri_ipc::Action::ToggleWindowedFullscreen { id: None } => {
                Self::ToggleWindowedFullscreen
            }
            tiri_ipc::Action::ToggleWindowedFullscreen { id: Some(id) } => {
                Self::ToggleWindowedFullscreenById(id)
            }
            tiri_ipc::Action::SetWindowFullscreen { id: None } => Self::SetWindowFullscreen,
            tiri_ipc::Action::SetWindowFullscreen { id: Some(id) } => {
                Self::SetWindowFullscreenById(id)
            }
            tiri_ipc::Action::UnsetWindowFullscreen { id: None } => Self::UnsetWindowFullscreen,
            tiri_ipc::Action::UnsetWindowFullscreen { id: Some(id) } => {
                Self::UnsetWindowFullscreenById(id)
            }
            tiri_ipc::Action::SetWindowMaximizedToEdges { id: None } => {
                Self::SetWindowMaximizedToEdges
            }
            tiri_ipc::Action::SetWindowMaximizedToEdges { id: Some(id) } => {
                Self::SetWindowMaximizedToEdgesById(id)
            }
            tiri_ipc::Action::UnsetWindowMaximizedToEdges { id: None } => {
                Self::UnsetWindowMaximizedToEdges
            }
            tiri_ipc::Action::UnsetWindowMaximizedToEdges { id: Some(id) } => {
                Self::UnsetWindowMaximizedToEdgesById(id)
            }
            tiri_ipc::Action::SetWindowedFullscreen { id: None } => Self::SetWindowedFullscreen,
            tiri_ipc::Action::SetWindowedFullscreen { id: Some(id) } => {
                Self::SetWindowedFullscreenById(id)
            }
            tiri_ipc::Action::UnsetWindowedFullscreen { id: None } => Self::UnsetWindowedFullscreen,
            tiri_ipc::Action::UnsetWindowedFullscreen { id: Some(id) } => {
                Self::UnsetWindowedFullscreenById(id)
            }
            tiri_ipc::Action::MoveWindowToScratchpad { id: None } => Self::MoveWindowToScratchpad,
            tiri_ipc::Action::MoveWindowToScratchpad { id: Some(id) } => {
                Self::MoveWindowToScratchpadById(id)
            }
            tiri_ipc::Action::ScratchpadShow {} => Self::ScratchpadShow,
            tiri_ipc::Action::Mark { name, mode } => match mode {
                tiri_ipc::MarkMode::Replace => Self::Mark(name),
                tiri_ipc::MarkMode::Add => Self::MarkAdd(name),
                tiri_ipc::MarkMode::Toggle => Self::MarkToggle(name),
            },
            tiri_ipc::Action::Unmark { name } => Self::Unmark(name),
            tiri_ipc::Action::FocusWindow { id } => Self::FocusWindow(id),
            tiri_ipc::Action::FocusWindowInColumn { index } => Self::FocusWindowInColumn(index),
            tiri_ipc::Action::FocusWindowPrevious { scope, filter } => Self::FocusWindowPrevious(
                match scope {
                    None | Some(tiri_ipc::RecentWindowsScope::All) => MruScope::All,
                    Some(tiri_ipc::RecentWindowsScope::Output) => MruScope::Output,
                    Some(tiri_ipc::RecentWindowsScope::Workspace) => MruScope::Workspace,
                },
                match filter {
                    None | Some(tiri_ipc::RecentWindowsFilter::All) => MruFilter::All,
                    Some(tiri_ipc::RecentWindowsFilter::AppId) => MruFilter::AppId,
                },
            ),
            tiri_ipc::Action::FocusColumnLeft {} => Self::FocusColumnLeft,
            tiri_ipc::Action::FocusColumnRight {} => Self::FocusColumnRight,
            tiri_ipc::Action::FocusContainerLeft {} => Self::FocusContainerLeft,
            tiri_ipc::Action::FocusContainerDown {} => Self::FocusContainerDown,
            tiri_ipc::Action::FocusContainerUp {} => Self::FocusContainerUp,
            tiri_ipc::Action::FocusContainerRight {} => Self::FocusContainerRight,
            tiri_ipc::Action::FocusColumnFirst {} => Self::FocusColumnFirst,
            tiri_ipc::Action::FocusColumnLast {} => Self::FocusColumnLast,
            tiri_ipc::Action::FocusColumnRightOrFirst {} => Self::FocusColumnRightOrFirst,
            tiri_ipc::Action::FocusColumnLeftOrLast {} => Self::FocusColumnLeftOrLast,
            tiri_ipc::Action::FocusColumn { index } => Self::FocusColumn(index),
            tiri_ipc::Action::FocusWindowOrMonitorUp {} => Self::FocusWindowOrMonitorUp,
            tiri_ipc::Action::FocusWindowOrMonitorDown {} => Self::FocusWindowOrMonitorDown,
            tiri_ipc::Action::FocusColumnOrMonitorLeft {} => Self::FocusColumnOrMonitorLeft,
            tiri_ipc::Action::FocusColumnOrMonitorRight {} => Self::FocusColumnOrMonitorRight,
            tiri_ipc::Action::FocusWindowDown {} => Self::FocusWindowDown,
            tiri_ipc::Action::FocusWindowUp {} => Self::FocusWindowUp,
            tiri_ipc::Action::FocusWindowDownOrColumnLeft {} => Self::FocusWindowDownOrColumnLeft,
            tiri_ipc::Action::FocusWindowDownOrColumnRight {} => Self::FocusWindowDownOrColumnRight,
            tiri_ipc::Action::FocusWindowUpOrColumnLeft {} => Self::FocusWindowUpOrColumnLeft,
            tiri_ipc::Action::FocusWindowUpOrColumnRight {} => Self::FocusWindowUpOrColumnRight,
            tiri_ipc::Action::FocusWindowOrWorkspaceDown {} => Self::FocusWindowOrWorkspaceDown,
            tiri_ipc::Action::FocusWindowOrWorkspaceUp {} => Self::FocusWindowOrWorkspaceUp,
            tiri_ipc::Action::FocusWindowTop {} => Self::FocusWindowTop,
            tiri_ipc::Action::FocusWindowBottom {} => Self::FocusWindowBottom,
            tiri_ipc::Action::FocusWindowDownOrTop {} => Self::FocusWindowDownOrTop,
            tiri_ipc::Action::FocusWindowUpOrBottom {} => Self::FocusWindowUpOrBottom,
            tiri_ipc::Action::MoveColumnLeft {} | tiri_ipc::Action::MoveContainerLeft {} => {
                Self::MoveContainerLeft
            }
            tiri_ipc::Action::MoveContainerDown {} => Self::MoveContainerDown,
            tiri_ipc::Action::MoveContainerUp {} => Self::MoveContainerUp,
            tiri_ipc::Action::MoveColumnRight {} | tiri_ipc::Action::MoveContainerRight {} => {
                Self::MoveContainerRight
            }
            tiri_ipc::Action::MoveColumnToFirst {} | tiri_ipc::Action::MoveContainerToFirst {} => {
                Self::MoveContainerToFirst
            }
            tiri_ipc::Action::MoveColumnToLast {} | tiri_ipc::Action::MoveContainerToLast {} => {
                Self::MoveContainerToLast
            }
            tiri_ipc::Action::MoveColumnToIndex { index }
            | tiri_ipc::Action::MoveContainerToIndex { index } => Self::MoveContainerToIndex(index),
            tiri_ipc::Action::MoveColumnLeftOrToMonitorLeft {}
            | tiri_ipc::Action::MoveContainerLeftOrToMonitorLeft {} => {
                Self::MoveContainerLeftOrToMonitorLeft
            }
            tiri_ipc::Action::MoveColumnRightOrToMonitorRight {}
            | tiri_ipc::Action::MoveContainerRightOrToMonitorRight {} => {
                Self::MoveContainerRightOrToMonitorRight
            }
            tiri_ipc::Action::MoveWindowDown {} => Self::MoveWindowDown,
            tiri_ipc::Action::MoveWindowUp {} => Self::MoveWindowUp,
            tiri_ipc::Action::MoveWindowDownOrToWorkspaceDown {} => {
                Self::MoveWindowDownOrToWorkspaceDown
            }
            tiri_ipc::Action::MoveWindowUpOrToWorkspaceUp {} => Self::MoveWindowUpOrToWorkspaceUp,
            tiri_ipc::Action::FocusParent {} => Self::FocusParent,
            tiri_ipc::Action::FocusChild {} => Self::FocusChild,
            tiri_ipc::Action::SplitHorizontal {} => Self::SplitHorizontal,
            tiri_ipc::Action::SplitVertical {} => Self::SplitVertical,
            tiri_ipc::Action::SplitToggle {} => Self::SplitToggle,
            tiri_ipc::Action::SplitNone {} => Self::SplitNone,
            tiri_ipc::Action::ToggleAutotile {} => Self::ToggleAutotile,
            tiri_ipc::Action::SetLayoutSplitH {} => Self::SetLayoutSplitH,
            tiri_ipc::Action::SetLayoutSplitV {} => Self::SetLayoutSplitV,
            tiri_ipc::Action::ToggleSplitLayout {} => Self::ToggleSplitLayout,
            tiri_ipc::Action::SetLayoutDefault {} => Self::SetLayoutDefault,
            tiri_ipc::Action::ToggleLayout { layouts } => Self::ToggleLayout(layouts),
            tiri_ipc::Action::SetLayoutStacked {} => Self::SetLayoutStacked,
            tiri_ipc::Action::SetLayoutTabbed {} => Self::SetLayoutTabbed,
            tiri_ipc::Action::ConsumeOrExpelWindowLeft { id: None } => {
                Self::ConsumeOrExpelWindowLeft
            }
            tiri_ipc::Action::ConsumeOrExpelWindowLeft { id: Some(id) } => {
                Self::ConsumeOrExpelWindowLeftById(id)
            }
            tiri_ipc::Action::ConsumeOrExpelWindowRight { id: None } => {
                Self::ConsumeOrExpelWindowRight
            }
            tiri_ipc::Action::ConsumeOrExpelWindowRight { id: Some(id) } => {
                Self::ConsumeOrExpelWindowRightById(id)
            }
            tiri_ipc::Action::ConsumeWindowIntoColumn {}
            | tiri_ipc::Action::ConsumeWindowIntoContainer {} => Self::ConsumeWindowIntoContainer,
            tiri_ipc::Action::ExpelWindowFromColumn {}
            | tiri_ipc::Action::ExpelWindowFromContainer {} => Self::ExpelWindowFromContainer,
            tiri_ipc::Action::SwapWindowWithMark { mark } => Self::SwapWindowWithMark(mark),
            tiri_ipc::Action::SwapWindowWithId { id } => Self::SwapWindowWithId(id),
            tiri_ipc::Action::ToggleColumnTabbedDisplay {} => Self::ToggleColumnTabbedDisplay,
            tiri_ipc::Action::SetColumnDisplay { display } => Self::SetColumnDisplay(display),
            tiri_ipc::Action::ToggleLayoutAll {} => Self::ToggleLayoutAll,
            tiri_ipc::Action::CenterWindow { id: None } => Self::CenterWindow,
            tiri_ipc::Action::CenterWindow { id: Some(id) } => Self::CenterWindowById(id),
            tiri_ipc::Action::FocusWorkspaceDown {} => Self::FocusWorkspaceDown,
            tiri_ipc::Action::FocusWorkspaceUp {} => Self::FocusWorkspaceUp,
            tiri_ipc::Action::FocusWorkspace { reference } => {
                Self::FocusWorkspace(WorkspaceReference::from(reference))
            }
            tiri_ipc::Action::FocusWorkspacePrevious {} => Self::FocusWorkspacePrevious,
            tiri_ipc::Action::MoveWindowToWorkspaceDown { focus } => {
                Self::MoveWindowToWorkspaceDown(focus)
            }
            tiri_ipc::Action::MoveWindowToWorkspaceUp { focus } => {
                Self::MoveWindowToWorkspaceUp(focus)
            }
            tiri_ipc::Action::MoveWindowToWorkspace {
                window_id: None,
                reference,
                focus,
            } => Self::MoveWindowToWorkspace(WorkspaceReference::from(reference), focus),
            tiri_ipc::Action::MoveWindowToWorkspace {
                window_id: Some(window_id),
                reference,
                focus,
            } => Self::MoveWindowToWorkspaceById {
                window_id,
                reference: WorkspaceReference::from(reference),
                focus,
            },
            tiri_ipc::Action::MoveColumnToWorkspaceDown { focus } => {
                Self::MoveColumnToWorkspaceDown(focus)
            }
            tiri_ipc::Action::MoveContainerToWorkspaceDown { focus } => {
                Self::MoveContainerToWorkspaceDown(focus)
            }
            tiri_ipc::Action::MoveColumnToWorkspaceUp { focus } => {
                Self::MoveColumnToWorkspaceUp(focus)
            }
            tiri_ipc::Action::MoveContainerToWorkspaceUp { focus } => {
                Self::MoveContainerToWorkspaceUp(focus)
            }
            tiri_ipc::Action::MoveColumnToWorkspace { reference, focus } => {
                Self::MoveColumnToWorkspace(WorkspaceReference::from(reference), focus)
            }
            tiri_ipc::Action::MoveContainerToWorkspace { reference, focus } => {
                Self::MoveContainerToWorkspace(WorkspaceReference::from(reference), focus)
            }
            tiri_ipc::Action::MoveWorkspaceDown {} => Self::MoveWorkspaceDown,
            tiri_ipc::Action::MoveWorkspaceUp {} => Self::MoveWorkspaceUp,
            tiri_ipc::Action::SetWorkspaceName {
                name,
                workspace: None,
            } => Self::SetWorkspaceName(name),
            tiri_ipc::Action::SetWorkspaceName {
                name,
                workspace: Some(reference),
            } => Self::SetWorkspaceNameByRef {
                name,
                reference: WorkspaceReference::from(reference),
            },
            tiri_ipc::Action::UnsetWorkspaceName { reference: None } => Self::UnsetWorkspaceName,
            tiri_ipc::Action::UnsetWorkspaceName {
                reference: Some(reference),
            } => Self::UnsetWorkSpaceNameByRef(WorkspaceReference::from(reference)),
            tiri_ipc::Action::FocusMonitorLeft {} => Self::FocusMonitorLeft,
            tiri_ipc::Action::FocusMonitorRight {} => Self::FocusMonitorRight,
            tiri_ipc::Action::FocusMonitorDown {} => Self::FocusMonitorDown,
            tiri_ipc::Action::FocusMonitorUp {} => Self::FocusMonitorUp,
            tiri_ipc::Action::FocusMonitorPrevious {} => Self::FocusMonitorPrevious,
            tiri_ipc::Action::FocusMonitorNext {} => Self::FocusMonitorNext,
            tiri_ipc::Action::FocusMonitor { output } => Self::FocusMonitor(output),
            tiri_ipc::Action::MoveWindowToMonitorLeft {} => Self::MoveWindowToMonitorLeft,
            tiri_ipc::Action::MoveWindowToMonitorRight {} => Self::MoveWindowToMonitorRight,
            tiri_ipc::Action::MoveWindowToMonitorDown {} => Self::MoveWindowToMonitorDown,
            tiri_ipc::Action::MoveWindowToMonitorUp {} => Self::MoveWindowToMonitorUp,
            tiri_ipc::Action::MoveWindowToMonitorPrevious {} => Self::MoveWindowToMonitorPrevious,
            tiri_ipc::Action::MoveWindowToMonitorNext {} => Self::MoveWindowToMonitorNext,
            tiri_ipc::Action::MoveWindowToMonitor { id: None, output } => {
                Self::MoveWindowToMonitor(output)
            }
            tiri_ipc::Action::MoveWindowToMonitor {
                id: Some(id),
                output,
            } => Self::MoveWindowToMonitorById { id, output },
            tiri_ipc::Action::MoveColumnToMonitorLeft {} => Self::MoveColumnToMonitorLeft,
            tiri_ipc::Action::MoveContainerToMonitorLeft {} => Self::MoveContainerToMonitorLeft,
            tiri_ipc::Action::MoveColumnToMonitorRight {} => Self::MoveColumnToMonitorRight,
            tiri_ipc::Action::MoveContainerToMonitorRight {} => Self::MoveContainerToMonitorRight,
            tiri_ipc::Action::MoveColumnToMonitorDown {} => Self::MoveColumnToMonitorDown,
            tiri_ipc::Action::MoveContainerToMonitorDown {} => Self::MoveContainerToMonitorDown,
            tiri_ipc::Action::MoveColumnToMonitorUp {} => Self::MoveColumnToMonitorUp,
            tiri_ipc::Action::MoveContainerToMonitorUp {} => Self::MoveContainerToMonitorUp,
            tiri_ipc::Action::MoveColumnToMonitorPrevious {} => Self::MoveColumnToMonitorPrevious,
            tiri_ipc::Action::MoveContainerToMonitorPrevious {} => {
                Self::MoveContainerToMonitorPrevious
            }
            tiri_ipc::Action::MoveColumnToMonitorNext {} => Self::MoveColumnToMonitorNext,
            tiri_ipc::Action::MoveContainerToMonitorNext {} => Self::MoveContainerToMonitorNext,
            tiri_ipc::Action::MoveColumnToMonitor { output } => Self::MoveColumnToMonitor(output),
            tiri_ipc::Action::MoveContainerToMonitor { output } => {
                Self::MoveContainerToMonitor(output)
            }
            tiri_ipc::Action::ResizeGrowWidth {} => Self::ResizeGrowWidth,
            tiri_ipc::Action::ResizeShrinkWidth {} => Self::ResizeShrinkWidth,
            tiri_ipc::Action::ResizeGrowHeight {} => Self::ResizeGrowHeight,
            tiri_ipc::Action::ResizeShrinkHeight {} => Self::ResizeShrinkHeight,
            tiri_ipc::Action::ResizeGrowLeft {} => Self::ResizeGrowLeft,
            tiri_ipc::Action::ResizeShrinkLeft {} => Self::ResizeShrinkLeft,
            tiri_ipc::Action::ResizeGrowRight {} => Self::ResizeGrowRight,
            tiri_ipc::Action::ResizeShrinkRight {} => Self::ResizeShrinkRight,
            tiri_ipc::Action::ResizeGrowUp {} => Self::ResizeGrowUp,
            tiri_ipc::Action::ResizeShrinkUp {} => Self::ResizeShrinkUp,
            tiri_ipc::Action::ResizeGrowDown {} => Self::ResizeGrowDown,
            tiri_ipc::Action::ResizeShrinkDown {} => Self::ResizeShrinkDown,
            tiri_ipc::Action::SetWindowWidth { id: None, change } => Self::SetWindowWidth(change),
            tiri_ipc::Action::SetWindowWidth {
                id: Some(id),
                change,
            } => Self::SetWindowWidthById { id, change },
            tiri_ipc::Action::SetWindowHeight { id: None, change } => Self::SetWindowHeight(change),
            tiri_ipc::Action::SetWindowHeight {
                id: Some(id),
                change,
            } => Self::SetWindowHeightById { id, change },
            tiri_ipc::Action::ResetWindowHeight { id: None } => Self::ResetWindowHeight,
            tiri_ipc::Action::ResetWindowHeight { id: Some(id) } => Self::ResetWindowHeightById(id),
            tiri_ipc::Action::SwitchPresetColumnWidth {} => Self::SwitchPresetColumnWidth,
            tiri_ipc::Action::SwitchPresetColumnWidthBack {} => Self::SwitchPresetColumnWidthBack,
            tiri_ipc::Action::SwitchPresetWindowWidth { id: None } => Self::SwitchPresetWindowWidth,
            tiri_ipc::Action::SwitchPresetWindowWidthBack { id: None } => {
                Self::SwitchPresetWindowWidthBack
            }
            tiri_ipc::Action::SwitchPresetWindowWidth { id: Some(id) } => {
                Self::SwitchPresetWindowWidthById(id)
            }
            tiri_ipc::Action::SwitchPresetWindowWidthBack { id: Some(id) } => {
                Self::SwitchPresetWindowWidthBackById(id)
            }
            tiri_ipc::Action::SwitchPresetWindowHeight { id: None } => {
                Self::SwitchPresetWindowHeight
            }
            tiri_ipc::Action::SwitchPresetWindowHeightBack { id: None } => {
                Self::SwitchPresetWindowHeightBack
            }
            tiri_ipc::Action::SwitchPresetWindowHeight { id: Some(id) } => {
                Self::SwitchPresetWindowHeightById(id)
            }
            tiri_ipc::Action::SwitchPresetWindowHeightBack { id: Some(id) } => {
                Self::SwitchPresetWindowHeightBackById(id)
            }
            tiri_ipc::Action::SetColumnWidth { change } => Self::SetColumnWidth(change),
            tiri_ipc::Action::ExpandColumnToAvailableWidth {} => Self::ExpandColumnToAvailableWidth,
            tiri_ipc::Action::SwitchLayout { layout } => Self::SwitchLayout(layout),
            tiri_ipc::Action::ShowHotkeyOverlay {} => Self::ShowHotkeyOverlay,
            tiri_ipc::Action::MoveWorkspaceToMonitorLeft {} => Self::MoveWorkspaceToMonitorLeft,
            tiri_ipc::Action::MoveWorkspaceToMonitorRight {} => Self::MoveWorkspaceToMonitorRight,
            tiri_ipc::Action::MoveWorkspaceToMonitorDown {} => Self::MoveWorkspaceToMonitorDown,
            tiri_ipc::Action::MoveWorkspaceToMonitorUp {} => Self::MoveWorkspaceToMonitorUp,
            tiri_ipc::Action::MoveWorkspaceToMonitorPrevious {} => {
                Self::MoveWorkspaceToMonitorPrevious
            }
            tiri_ipc::Action::MoveWorkspaceToIndex {
                index,
                reference: Some(reference),
            } => Self::MoveWorkspaceToIndexByRef {
                new_idx: index,
                reference: WorkspaceReference::from(reference),
            },
            tiri_ipc::Action::MoveWorkspaceToIndex {
                index,
                reference: None,
            } => Self::MoveWorkspaceToIndex(index),
            tiri_ipc::Action::MoveWorkspaceToMonitor {
                output,
                reference: Some(reference),
            } => Self::MoveWorkspaceToMonitorByRef {
                output_name: output,
                reference: WorkspaceReference::from(reference),
            },
            tiri_ipc::Action::MoveWorkspaceToMonitor {
                output,
                reference: None,
            } => Self::MoveWorkspaceToMonitor(output),
            tiri_ipc::Action::MoveWorkspaceToMonitorNext {} => Self::MoveWorkspaceToMonitorNext,
            tiri_ipc::Action::ToggleDebugTint {} => Self::ToggleDebugTint,
            tiri_ipc::Action::DebugToggleOpaqueRegions {} => Self::DebugToggleOpaqueRegions,
            tiri_ipc::Action::DebugToggleDamage {} => Self::DebugToggleDamage,
            tiri_ipc::Action::ToggleTouchpad {} => Self::ToggleTouchpad,
            tiri_ipc::Action::ToggleWindowFloating { id: None } => Self::ToggleWindowFloating,
            tiri_ipc::Action::ToggleWindowFloating { id: Some(id) } => {
                Self::ToggleWindowFloatingById(id)
            }
            tiri_ipc::Action::ToggleWindowSticky { id: None } => Self::ToggleWindowSticky,
            tiri_ipc::Action::ToggleWindowSticky { id: Some(id) } => {
                Self::ToggleWindowStickyById(id)
            }
            tiri_ipc::Action::MoveWindowToFloating { id: None } => Self::MoveWindowToFloating,
            tiri_ipc::Action::MoveWindowToFloating { id: Some(id) } => {
                Self::MoveWindowToFloatingById(id)
            }
            tiri_ipc::Action::MoveWindowToTiling { id: None } => Self::MoveWindowToTiling,
            tiri_ipc::Action::MoveWindowToTiling { id: Some(id) } => {
                Self::MoveWindowToTilingById(id)
            }
            tiri_ipc::Action::FocusFloating {} => Self::FocusFloating,
            tiri_ipc::Action::FocusTiling {} => Self::FocusTiling,
            tiri_ipc::Action::SwitchFocusBetweenFloatingAndTiling {} => {
                Self::SwitchFocusBetweenFloatingAndTiling
            }
            tiri_ipc::Action::MoveFloatingWindow { id, x, y } => {
                Self::MoveFloatingWindowById { id, x, y }
            }
            tiri_ipc::Action::ToggleWindowRuleOpacity { id: None } => Self::ToggleWindowRuleOpacity,
            tiri_ipc::Action::ToggleWindowRuleOpacity { id: Some(id) } => {
                Self::ToggleWindowRuleOpacityById(id)
            }
            tiri_ipc::Action::SetDynamicCastWindow { id: None } => Self::SetDynamicCastWindow,
            tiri_ipc::Action::SetDynamicCastWindow { id: Some(id) } => {
                Self::SetDynamicCastWindowById(id)
            }
            tiri_ipc::Action::SetDynamicCastMonitor { output } => {
                Self::SetDynamicCastMonitor(output)
            }
            tiri_ipc::Action::ClearDynamicCastTarget {} => Self::ClearDynamicCastTarget,
            tiri_ipc::Action::StopCast { session_id } => Self::StopCast(session_id),
            tiri_ipc::Action::ToggleOverview {} => Self::ToggleOverview,
            tiri_ipc::Action::OpenOverview {} => Self::OpenOverview,
            tiri_ipc::Action::CloseOverview {} => Self::CloseOverview,
            tiri_ipc::Action::ToggleWindowUrgent { id } => Self::ToggleWindowUrgent(id),
            tiri_ipc::Action::SetWindowUrgent { id } => Self::SetWindowUrgent(id),
            tiri_ipc::Action::UnsetWindowUrgent { id } => Self::UnsetWindowUrgent(id),
            tiri_ipc::Action::LoadConfigFile { path } => Self::LoadConfigFile(path),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum WorkspaceReference {
    Id(u64),
    Index(u32),
    Name(String),
}

impl From<WorkspaceReferenceArg> for WorkspaceReference {
    fn from(reference: WorkspaceReferenceArg) -> WorkspaceReference {
        match reference {
            WorkspaceReferenceArg::Id(id) => Self::Id(id),
            WorkspaceReferenceArg::Index(i) => Self::Index(i),
            WorkspaceReferenceArg::Name(n) => Self::Name(n),
        }
    }
}

impl<S: knuffel::traits::ErrorSpan> knuffel::DecodeScalar<S> for WorkspaceReference {
    fn type_check(
        type_name: &Option<knuffel::span::Spanned<knuffel::ast::TypeName, S>>,
        ctx: &mut knuffel::decode::Context<S>,
    ) {
        if let Some(type_name) = &type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for this node",
            ));
        }
    }

    fn raw_decode(
        val: &knuffel::span::Spanned<knuffel::ast::Literal, S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<WorkspaceReference, DecodeError<S>> {
        match &**val {
            knuffel::ast::Literal::String(ref s) => Ok(WorkspaceReference::Name(s.clone().into())),
            knuffel::ast::Literal::Int(ref value) => match value.try_into() {
                Ok(v) => Ok(WorkspaceReference::Index(v)),
                Err(e) => {
                    ctx.emit_error(DecodeError::conversion(val, e));
                    Ok(WorkspaceReference::Index(0))
                }
            },
            _ => {
                ctx.emit_error(DecodeError::unsupported(
                    val,
                    "Unsupported value, only numbers and strings are recognized",
                ));
                Ok(WorkspaceReference::Index(0))
            }
        }
    }
}

impl<S> knuffel::Decode<S> for Binds
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        expect_only_children(node, ctx);

        let mut nodes: Vec<&knuffel::ast::SpannedNode<S>> = Vec::new();
        let mut binds: Vec<Bind> = Vec::new();

        for child in node.children() {
            match Bind::decode_node(child, ctx) {
                Err(e) => {
                    ctx.emit_error(e);
                }
                Ok(bind) => {
                    let conflict = binds
                        .iter()
                        .position(|prev| prev.key.conflicts_with(&bind.key));
                    if let Some(idx) = conflict {
                        // Even though it's technically incorrect, we use
                        // `DecodeError::Missing` here because it labels the bind with
                        // "node starts here", which is the least bad option
                        ctx.emit_error(DecodeError::missing(
                            nodes[idx],
                            "keybind first defined here",
                        ));

                        ctx.emit_error(DecodeError::unexpected(
                            &child.node_name,
                            "keybind",
                            if binds[idx].key == bind.key {
                                "duplicate keybind later defined here"
                            } else {
                                "conflicting keybind later defined here"
                            },
                        ));
                    } else {
                        nodes.push(child);
                        binds.push(bind);
                    }
                }
            }
        }

        Ok(Self(binds))
    }
}

impl<S> knuffel::Decode<S> for ModeBinds
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        if let Some(type_name) = &node.type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for this node",
            ));
        }

        let mut args = node.arguments.iter();
        let name_val = args
            .next()
            .ok_or_else(|| DecodeError::missing(node, "mode name argument is required"))?;
        let name: String = knuffel::traits::DecodeScalar::decode(name_val, ctx)?;

        if let Some(extra) = args.next() {
            ctx.emit_error(DecodeError::unexpected(
                &extra.literal,
                "argument",
                "unexpected extra argument",
            ));
        }

        for name in node.properties.keys() {
            ctx.emit_error(DecodeError::unexpected(
                name,
                "property",
                "no properties expected for this node",
            ));
        }

        let mut seen_keys = HashSet::new();
        let mut binds = Vec::new();

        for child in node.children() {
            if &**child.node_name == "binds" {
                match Binds::decode_node(child, ctx) {
                    Err(e) => ctx.emit_error(e),
                    Ok(part) => {
                        for bind in part.0 {
                            if seen_keys.insert(bind.key) {
                                binds.push(bind);
                            } else {
                                ctx.emit_error(DecodeError::unexpected(
                                    &child.node_name,
                                    "keybind",
                                    "duplicate keybind",
                                ));
                            }
                        }
                    }
                }
                continue;
            }

            match Bind::decode_node(child, ctx) {
                Err(e) => ctx.emit_error(e),
                Ok(bind) => {
                    if seen_keys.insert(bind.key) {
                        binds.push(bind);
                    } else {
                        ctx.emit_error(DecodeError::unexpected(
                            &child.node_name,
                            "keybind",
                            "duplicate keybind",
                        ));
                    }
                }
            }
        }

        Ok(Self {
            name,
            binds: Binds(binds),
        })
    }
}

impl<S> knuffel::Decode<S> for Bind
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        if let Some(type_name) = &node.type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for this node",
            ));
        }

        for val in node.arguments.iter() {
            ctx.emit_error(DecodeError::unexpected(
                &val.literal,
                "argument",
                "no arguments expected for this node",
            ));
        }

        let key = node
            .node_name
            .parse::<Key>()
            .map_err(|e| DecodeError::conversion(&node.node_name, e.wrap_err("invalid keybind")))?;

        let mut repeat = None;
        let mut repeat_node = None;
        let mut pass_through = None;
        let mut cooldown = None;
        let mut allow_when_locked = false;
        let mut allow_when_locked_node = None;
        let mut allow_inhibiting = true;
        let mut hotkey_overlay_title = None;

        for (name, val) in &node.properties {
            match &***name {
                "repeat" => {
                    repeat = Some(knuffel::traits::DecodeScalar::decode(val, ctx)?);
                    repeat_node = Some(name);
                }
                "pass-through" => {
                    pass_through = Some(knuffel::traits::DecodeScalar::decode(val, ctx)?);

                    // Supporting this for mouse binds would be a bit weird. Mouse binds trigger
                    // compositor gestures, so should pass-through also trigger the corresponding
                    // gesture, on top of the bind? If yes, then there are some conflicts, e.g. with
                    // toggle-overview and the Mod+LMB move grab or LMB press-to-close-overview.
                    //
                    // All in all, this needs design, and I'm not sure why you'd want pass-through
                    // mouse binds anyway, so, let's do keyboard only for now.
                    if !key.trigger.is_keyboard() {
                        ctx.emit_error(DecodeError::unexpected(
                            name,
                            "property",
                            "pass-through is only supported for keyboard binds",
                        ));
                    }
                }
                "cooldown-ms" => {
                    cooldown = Some(Duration::from_millis(
                        knuffel::traits::DecodeScalar::decode(val, ctx)?,
                    ));
                }
                "allow-when-locked" => {
                    allow_when_locked = knuffel::traits::DecodeScalar::decode(val, ctx)?;
                    allow_when_locked_node = Some(name);
                }
                "allow-inhibiting" => {
                    allow_inhibiting = knuffel::traits::DecodeScalar::decode(val, ctx)?;
                }
                "hotkey-overlay-title" => {
                    hotkey_overlay_title = Some(knuffel::traits::DecodeScalar::decode(val, ctx)?);
                }
                name_str => {
                    ctx.emit_error(DecodeError::unexpected(
                        name,
                        "property",
                        format!("unexpected property `{}`", name_str.escape_default()),
                    ));
                }
            }
        }

        // If the action is invalid but the key is fine, we still want to return something.
        // That way, the parent can handle the existence of duplicate keybinds,
        // even if their contents are not valid.
        let dummy = Self {
            key,
            action: BoundAction::Press(Action::Spawn(vec![])),
            repeat: true,
            pass_through: None,
            cooldown: None,
            allow_when_locked: false,
            allow_inhibiting: true,
            hotkey_overlay_title: None,
        };

        // direct_action is an action node that is not wrapped in a press or release section
        let mut direct_action: Option<Action> = None;
        let mut press_action: Option<Action> = None;
        let mut release_action: Option<Action> = None;

        let decode_action = |ctx: &mut knuffel::decode::Context<S>,
                             child: &knuffel::ast::SpannedNode<S>| {
            match Action::decode_node(child, ctx) {
                Ok(action) => Some(action),
                Err(e) => {
                    ctx.emit_error(e);
                    None
                }
            }
        };

        for child in node.children() {
            let child_name = &*child.node_name;
            let child_name: &str = child_name.as_ref();

            if child_name == "press" || child_name == "release" {
                let is_press = child_name == "press";
                let slot = if is_press {
                    &mut press_action
                } else {
                    &mut release_action
                };

                if slot.is_some() {
                    ctx.emit_error(DecodeError::unexpected(
                        &child.node_name,
                        "section",
                        format!("duplicate `{child_name}` section"),
                    ));
                    continue;
                }

                if direct_action.is_some() {
                    ctx.emit_error(DecodeError::unexpected(
                        &child.node_name,
                        "section",
                        "cannot mix direct actions with press/release sections",
                    ));
                    continue;
                }

                // The action is the section's single child node.
                let mut children = child.children();
                let Some(action_child) = children.next() else {
                    ctx.emit_error(DecodeError::missing(
                        child,
                        format!("expected an action for this {child_name} section"),
                    ));
                    continue;
                };

                for unwanted_child in children {
                    ctx.emit_error(DecodeError::unexpected(
                        unwanted_child,
                        "node",
                        "only one action is allowed per keybind",
                    ));
                }

                *slot = decode_action(ctx, action_child);
            } else {
                if direct_action.is_some() {
                    ctx.emit_error(DecodeError::unexpected(
                        &child.node_name,
                        "node",
                        "only one action is allowed per keybind",
                    ));
                    continue;
                }

                if press_action.is_some() || release_action.is_some() {
                    ctx.emit_error(DecodeError::unexpected(
                        &child.node_name,
                        "node",
                        "cannot mix direct actions with press/release sections",
                    ));
                    continue;
                }

                direct_action = decode_action(ctx, child);
            }
        }

        let press_action = press_action.or(direct_action);

        if release_action.is_some() && key.trigger.is_scroll() {
            ctx.emit_error(DecodeError::unexpected(
                node,
                "bind",
                "release sections are not supported for scroll binds",
            ));
            release_action = None;
        }

        if let Some(node) = repeat_node {
            if repeat == Some(true) && press_action.is_none() && release_action.is_some() {
                ctx.emit_error(DecodeError::unexpected(
                    node,
                    "property",
                    "release-only binds cannot repeat",
                ));
            }
        }

        if let Some(node) = allow_when_locked_node {
            for action in press_action.iter().chain(release_action.iter()) {
                if !matches!(action, Action::Spawn(_) | Action::SpawnSh(_)) {
                    ctx.emit_error(DecodeError::unexpected(
                        node,
                        "property",
                        "allow-when-locked can only be set on spawn binds",
                    ));
                }
            }
        }

        // The toggle-inhibit action must always be uninhibitable.
        // Otherwise, it would be impossible to trigger it.
        if matches!(press_action, Some(Action::ToggleKeyboardShortcutsInhibit))
            || matches!(release_action, Some(Action::ToggleKeyboardShortcutsInhibit))
        {
            allow_inhibiting = false;
        }

        let repeat = match repeat {
            Some(value) => value,
            None => release_action.is_none(),
        };

        let action = match (press_action, release_action) {
            (Some(press), Some(release)) => BoundAction::Both { press, release },
            (Some(press), None) => BoundAction::Press(press),
            (None, Some(release)) => BoundAction::Release(release),
            (None, None) => {
                // If a press or release section was present, an error about its missing or
                // invalid action was already emitted above.
                let has_section = node.children().any(|child| {
                    let name = child.node_name.as_ref();
                    name == "press" || name == "release"
                });
                if !has_section {
                    ctx.emit_error(DecodeError::missing(
                        node,
                        "expected an action for this keybind",
                    ));
                }
                return Ok(dummy);
            }
        };

        Ok(Self {
            key,
            action,
            repeat,
            pass_through,
            cooldown,
            allow_when_locked,
            allow_inhibiting,
            hotkey_overlay_title,
        })
    }
}

impl FromStr for Key {
    type Err = miette::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains('\0') {
            return Err(miette!("invalid key with a null byte: {s:?}"));
        }

        let mut written_modifiers: Vec<(Modifiers, &str)> = Vec::new();

        let mut split = s.split('+');
        let key = split.next_back().unwrap();

        for part in split {
            let part = part.trim();
            let modifier = if part.eq_ignore_ascii_case("mod") {
                Modifiers::COMPOSITOR
            } else if part.eq_ignore_ascii_case("ctrl") || part.eq_ignore_ascii_case("control") {
                Modifiers::CTRL
            } else if part.eq_ignore_ascii_case("shift") {
                Modifiers::SHIFT
            } else if part.eq_ignore_ascii_case("alt") {
                Modifiers::ALT
            } else if part.eq_ignore_ascii_case("super") || part.eq_ignore_ascii_case("win") {
                Modifiers::SUPER
            } else if part.eq_ignore_ascii_case("iso_level3_shift")
                || part.eq_ignore_ascii_case("mod5")
            {
                Modifiers::ISO_LEVEL3_SHIFT
            } else if part.eq_ignore_ascii_case("iso_level5_shift")
                || part.eq_ignore_ascii_case("mod3")
            {
                Modifiers::ISO_LEVEL5_SHIFT
            } else {
                return Err(miette!("invalid modifier: {part}"));
            };

            written_modifiers.push((modifier, part));
        }

        let trigger = if key.eq_ignore_ascii_case("MouseLeft") {
            Trigger::MouseLeft
        } else if key.eq_ignore_ascii_case("MouseRight") {
            Trigger::MouseRight
        } else if key.eq_ignore_ascii_case("MouseMiddle") {
            Trigger::MouseMiddle
        } else if key.eq_ignore_ascii_case("MouseBack") {
            Trigger::MouseBack
        } else if key.eq_ignore_ascii_case("MouseForward") {
            Trigger::MouseForward
        } else if key.eq_ignore_ascii_case("WheelScrollDown") {
            Trigger::WheelScrollDown
        } else if key.eq_ignore_ascii_case("WheelScrollUp") {
            Trigger::WheelScrollUp
        } else if key.eq_ignore_ascii_case("WheelScrollLeft") {
            Trigger::WheelScrollLeft
        } else if key.eq_ignore_ascii_case("WheelScrollRight") {
            Trigger::WheelScrollRight
        } else if key.eq_ignore_ascii_case("TouchpadScrollDown") {
            Trigger::TouchpadScrollDown
        } else if key.eq_ignore_ascii_case("TouchpadScrollUp") {
            Trigger::TouchpadScrollUp
        } else if key.eq_ignore_ascii_case("TouchpadScrollLeft") {
            Trigger::TouchpadScrollLeft
        } else if key.eq_ignore_ascii_case("TouchpadScrollRight") {
            Trigger::TouchpadScrollRight
        } else if key.eq_ignore_ascii_case("TabletStylusButton1") {
            Trigger::TabletStylusButton1
        } else if key.eq_ignore_ascii_case("TabletStylusButton2") {
            Trigger::TabletStylusButton2
        } else if key.eq_ignore_ascii_case("TabletStylusButton3") {
            Trigger::TabletStylusButton3
        } else if key.eq_ignore_ascii_case("Mod") {
            Trigger::CompositorMod
        } else if key.eq_ignore_ascii_case("Ctrl") || key.eq_ignore_ascii_case("Control") {
            Trigger::Modifier(ModKey::Ctrl)
        } else if key.eq_ignore_ascii_case("Shift") {
            Trigger::Modifier(ModKey::Shift)
        } else if key.eq_ignore_ascii_case("Alt") {
            Trigger::Modifier(ModKey::Alt)
        } else if key.eq_ignore_ascii_case("Super") || key.eq_ignore_ascii_case("Win") {
            Trigger::Modifier(ModKey::Super)
        } else if key.eq_ignore_ascii_case("ISO_Level3_Shift") || key.eq_ignore_ascii_case("Mod5") {
            Trigger::Modifier(ModKey::IsoLevel3Shift)
        } else if key.eq_ignore_ascii_case("ISO_Level5_Shift") || key.eq_ignore_ascii_case("Mod3") {
            Trigger::Modifier(ModKey::IsoLevel5Shift)
        } else {
            let mut keysym = keysym_from_name(key, KEYSYM_CASE_INSENSITIVE);

            let with_xf86;
            let mut final_key = key;
            if keysym.raw() == KEY_NoSymbol {
                // If the key didn't match, try it with an XF86 prefix.
                // This way, you can write e.g. AudioMute instead of XF86AudioMute.
                //
                // This is actually needed for the xdg-desktop-portal-gnome implementation of the
                // GlobalShortcuts portal, as it will request non-XF86-prefixed names. Mutter then
                // does this same exact prefixing:
                // https://gitlab.gnome.org/GNOME/mutter/-/blob/b48811767189fb5446bd1b831e98271a21866e09/src/core/meta-accel-parse.c#L302-311
                with_xf86 = format!("XF86{key}");
                keysym = keysym_from_name(&with_xf86, KEYSYM_CASE_INSENSITIVE);
                if keysym.raw() != KEY_NoSymbol {
                    final_key = &with_xf86;
                }
            }

            // The keyboard event handling code can receive either
            // XF86ScreenSaver or XF86Screensaver, because there is no
            // case mapping defined between these keysyms. If we just
            // use the case-insensitive version of keysym_from_name it
            // is not possible to bind the uppercase version, because the
            // case-insensitive match prefers the lowercase version when
            // there is a choice.
            //
            // Therefore, when we match this key with the initial
            // case-insensitive match we try a further case-sensitive match
            // (so that either key can be bound). If that fails, we change
            // to the uppercase version because:
            //
            // - A comment in xkb_keysym_from_name (in libxkbcommon) tells us that the uppercase
            //   version is the "best" of the two. [0]
            // - The xkbcommon crate only has a constant for ScreenSaver. [1]
            //
            // [0]: https://github.com/xkbcommon/libxkbcommon/blob/45a118d5325b051343b4b174f60c1434196fa7d4/src/keysym.c#L276
            // [1]: https://docs.rs/xkbcommon/latest/xkbcommon/xkb/keysyms/index.html#:~:text=KEY%5FXF86ScreenSaver
            //
            // See https://github.com/niri-wm/niri/issues/1969
            if keysym == Keysym::XF86_Screensaver {
                keysym = keysym_from_name(final_key, KEYSYM_NO_FLAGS);
                if keysym.raw() == KEY_NoSymbol {
                    keysym = Keysym::XF86_ScreenSaver;
                }
            }
            if keysym.raw() == KEY_NoSymbol {
                return Err(miette!("invalid key: {key}"));
            }
            Trigger::Keysym(keysym)
        };

        let mut modifiers = trigger.implied_modifier();
        for (modifier, spelling) in written_modifiers {
            if modifiers.contains(modifier) {
                warn!("redundant modifier `{spelling}` in keybind `{s}`");
            }

            modifiers |= modifier;
        }

        Ok(Key { trigger, modifiers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_directional_resize_actions() {
        let config = crate::Config::parse_mem(
            r#"
binds {
    A { resize-grow-left; }
    B { resize-shrink-left; }
    C { resize-grow-right; }
    D { resize-shrink-right; }
    E { resize-grow-up; }
    F { resize-shrink-up; }
    G { resize-grow-down; }
    H { resize-shrink-down; }
}
"#,
        )
        .unwrap();

        let actions: Vec<_> = config.binds.0.into_iter().map(|bind| bind.action).collect();
        assert_eq!(
            actions,
            [
                Action::ResizeGrowLeft,
                Action::ResizeShrinkLeft,
                Action::ResizeGrowRight,
                Action::ResizeShrinkRight,
                Action::ResizeGrowUp,
                Action::ResizeShrinkUp,
                Action::ResizeGrowDown,
                Action::ResizeShrinkDown,
            ]
            .map(BoundAction::Press)
        );
    }

    #[test]
    fn parse_sway_split_and_layout_actions() {
        let config = crate::Config::parse_mem(
            r#"
binds {
    A { split-toggle; }
    B { split-none; }
    C { set-layout-default; }
    D { toggle-layout; }
    E { toggle-layout "splith" "tabbed" "stacking" "splitv"; }
    F { toggle-layout "tabbed"; }
}
"#,
        )
        .unwrap();

        let actions: Vec<_> = config.binds.0.into_iter().map(|bind| bind.action).collect();
        assert_eq!(
            actions,
            [
                Action::SplitToggle,
                Action::SplitNone,
                Action::SetLayoutDefault,
                Action::ToggleLayout(vec![]),
                Action::ToggleLayout(vec![
                    LayoutCycleTarget::Splith,
                    LayoutCycleTarget::Tabbed,
                    LayoutCycleTarget::Stacking,
                    LayoutCycleTarget::Splitv,
                ]),
                Action::ToggleLayout(vec![LayoutCycleTarget::Tabbed]),
            ]
            .map(BoundAction::Press)
        );
    }

    #[test]
    fn parse_xf86_screensaver() {
        assert_eq!(
            "XF86ScreenSaver".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::XF86_ScreenSaver),
                modifiers: Modifiers::empty(),
            },
        );
        assert_eq!(
            "XF86Screensaver".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::XF86_Screensaver),
                modifiers: Modifiers::empty(),
            }
        );
        assert_eq!(
            "xf86screensaver".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::XF86_ScreenSaver),
                modifiers: Modifiers::empty(),
            }
        );

        // And now with the auto-prepended XF86 prefix.
        assert_eq!(
            "ScreenSaver".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::XF86_ScreenSaver),
                modifiers: Modifiers::empty(),
            },
        );
        assert_eq!(
            "Screensaver".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::XF86_Screensaver),
                modifiers: Modifiers::empty(),
            }
        );
        assert_eq!(
            "screensaver".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::XF86_ScreenSaver),
                modifiers: Modifiers::empty(),
            }
        );
    }

    #[test]
    fn parse_iso_level_shifts() {
        assert_eq!(
            "ISO_Level3_Shift+A".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::a),
                modifiers: Modifiers::ISO_LEVEL3_SHIFT
            },
        );
        assert_eq!(
            "Mod5+A".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::a),
                modifiers: Modifiers::ISO_LEVEL3_SHIFT
            },
        );

        assert_eq!(
            "ISO_Level5_Shift+A".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::a),
                modifiers: Modifiers::ISO_LEVEL5_SHIFT
            },
        );
        assert_eq!(
            "Mod3+A".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::a),
                modifiers: Modifiers::ISO_LEVEL5_SHIFT
            },
        );
    }

    #[test]
    fn parse_scroll_triggers() {
        assert_eq!(
            "Mod+WheelScrollDown".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::WheelScrollDown,
                modifiers: Modifiers::COMPOSITOR,
            }
        );
        assert_eq!(
            "Ctrl+TouchpadScrollUp".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::TouchpadScrollUp,
                modifiers: Modifiers::CTRL,
            }
        );
    }

    #[test]
    fn parse_bare_modifier_spellings() {
        // Each modifier can be spelled with its canonical name or an alias. A bare modifier key
        // parses to `Trigger::Modifier`.
        for (text, modifier) in [
            ("Ctrl", ModKey::Ctrl),
            ("Control", ModKey::Ctrl),
            ("Shift", ModKey::Shift),
            ("Alt", ModKey::Alt),
            ("Super", ModKey::Super),
            ("Win", ModKey::Super),
            ("Mod5", ModKey::IsoLevel3Shift),
            ("ISO_Level3_Shift", ModKey::IsoLevel3Shift),
            ("Mod3", ModKey::IsoLevel5Shift),
            ("ISO_Level5_Shift", ModKey::IsoLevel5Shift),
        ] {
            assert_eq!(
                text.parse::<Key>().unwrap(),
                Key {
                    trigger: Trigger::Modifier(modifier),
                    modifiers: modifier.to_modifiers(),
                },
                "parsing `{text}`",
            );
        }
    }

    #[test]
    fn parse_mod_as_trigger() {
        // The mod key keeps its own trigger, which is resolved to a real modifier at runtime.
        assert_eq!(
            "Mod".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::CompositorMod,
                modifiers: Modifiers::COMPOSITOR,
            },
        );

        // It can also be used together with other held modifiers.
        assert_eq!(
            "Ctrl+Mod".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::CompositorMod,
                modifiers: Modifiers::CTRL | Modifiers::COMPOSITOR,
            },
        );

        // `Mod` also works as a held modifier of another trigger.
        assert_eq!(
            "Mod+Control_L".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::Control_L),
                modifiers: Modifiers::COMPOSITOR | Modifiers::CTRL,
            },
        );
    }

    #[test]
    fn parse_multi_modifier_triggers() {
        // A modifier key as the trigger with another held modifier, either spelled as the modifier
        // itself or as its keysym.
        assert_eq!(
            "Ctrl+Alt".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Modifier(ModKey::Alt),
                modifiers: Modifiers::CTRL | Modifiers::ALT,
            },
        );
        assert_eq!(
            "Ctrl+Alt_L".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::Alt_L),
                modifiers: Modifiers::CTRL | Modifiers::ALT,
            },
        );
    }

    #[test]
    fn parse_normalizes_redundant_modifiers() {
        let key = |text: &str| text.parse::<Key>().unwrap();

        // Modifier triggers can be written with or without the redundant modifier.
        assert_eq!(key("Alt+Alt_L"), key("Alt_L"));
        assert_eq!(key("Alt+Alt_R"), key("Alt_R"));
        assert_eq!(key("Alt+Alt"), key("Alt"));
        assert_eq!(key("Alt+Alt+Alt_L"), key("Alt_L"));
        assert_eq!(key("Ctrl+Shift+Control_L"), key("Shift+Control_L"));
        assert_eq!(key("Mod+Mod"), key("Mod"));

        // Duplicate modifiers are fine with non-modifier triggers.
        assert_eq!(key("Alt+Alt+Q"), key("Alt+Q"));
        assert_eq!(key("Mod+Mod+Q"), key("Mod+Q"));

        // Still works if spelled differently.
        assert_eq!(key("Ctrl+Control+Q"), key("Ctrl+Q"));
        assert_eq!(key("Super+Win+Q"), key("Super+Q"));
    }

    #[test]
    fn parse_modifier_trigger_includes_implied_modifier() {
        // The modifier that the trigger implies is part of the key.
        assert_eq!(
            "Alt_L".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Keysym(Keysym::Alt_L),
                modifiers: Modifiers::ALT,
            },
        );
        assert_eq!(
            "Alt+Alt".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::Modifier(ModKey::Alt),
                modifiers: Modifiers::ALT,
            },
        );
        // `Mod` is only resolved at runtime, so no trigger implies it.
        assert_eq!(
            "Alt+Mod".parse::<Key>().unwrap(),
            Key {
                trigger: Trigger::CompositorMod,
                modifiers: Modifiers::ALT | Modifiers::COMPOSITOR,
            },
        );
    }

    #[test]
    fn key_conflicts() {
        let key = |text: &str| text.parse::<Key>().unwrap();

        // Alt_L and Alt_R conflict with Alt, and vice versa.
        let alt = key("Alt");
        let alt_l = key("Alt_L");
        let alt_r = key("Alt_R");
        assert!(alt.conflicts_with(&alt_l));
        assert!(alt.conflicts_with(&alt_r));
        assert!(alt_l.conflicts_with(&alt));
        assert!(alt_r.conflicts_with(&alt));

        // The left and right keysyms are different keys.
        assert!(!alt_l.conflicts_with(&alt_r));

        // Different modifiers mean different binds, even for the same trigger.
        assert!(!alt.conflicts_with(&key("Ctrl+Alt_L")));

        // Non-overlapping triggers don't conflict, even with the same modifiers.
        assert!(!key("Mod+Q").conflicts_with(&key("Mod+L")));
        assert!(!key("Q").conflicts_with(&key("Ctrl")));

        // Identical keys are duplicates.
        assert!(key("Q").conflicts_with(&key("Q")));
        assert!(alt.conflicts_with(&alt));

        // The compositor mod key is only resolved at runtime, so `Mod` isn't compared against the
        // mod key's own spellings.
        let compositor_mod = key("Mod");
        assert!(!compositor_mod.conflicts_with(&key("Super_L")));
        assert!(!compositor_mod.conflicts_with(&key("Super")));

        // Generic-modifier-only keys are order-independent.
        assert!(key("Ctrl+Alt").conflicts_with(&key("Alt+Ctrl")));
        assert!(key("Mod+Shift+Alt").conflicts_with(&key("Shift+Alt+Mod")));
        assert!(key("Ctrl+Alt").conflicts_with(&key("Alt+Control_L")));
        assert!(key("Ctrl+Alt_L").conflicts_with(&key("Alt+Ctrl")));

        assert!(!key("Ctrl+Alt").conflicts_with(&key("Ctrl+Alt+A")));
    }

    #[test]
    fn null_byte() {
        // Shouldn't panic.
        let _ = "W\0".parse::<Key>();
    }
}
