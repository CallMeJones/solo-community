//! Main-window destinations and bounded back navigation.

/// How deep the back stack goes. Long enough that no realistic click path
/// runs out, short enough that it never grows without bound.
pub(super) const NAV_HISTORY_LIMIT: usize = 32;
/// Back stack for the main window.
///
/// Split out from the window struct so the rules — no self-entry, bounded
/// depth, Back falls through to Controls — are unit-testable without standing
/// up an entire egui app.
#[derive(Debug, Default)]
pub(super) struct NavHistory {
    stack: Vec<MainTab>,
}

impl NavHistory {
    /// Record `current` and return the tab to show. Re-selecting the current
    /// tab records nothing, so clicking "Settings" twice still needs one Back.
    pub(super) fn navigate(&mut self, current: MainTab, to: MainTab) -> MainTab {
        if current == to {
            return current;
        }
        self.stack.push(current);
        if self.stack.len() > NAV_HISTORY_LIMIT {
            self.stack.remove(0);
        }
        to
    }

    /// Retrace one step, or land on Controls when the trail is empty — Back is
    /// never a dead button.
    pub(super) fn back(&mut self) -> MainTab {
        self.stack.pop().unwrap_or(MainTab::Controls)
    }

    /// The trail exists only to get back to Controls, so arriving there clears it.
    pub(super) fn home(&mut self) -> MainTab {
        self.stack.clear();
        MainTab::Controls
    }

    pub(super) fn peek(&self) -> MainTab {
        self.stack.last().copied().unwrap_or(MainTab::Controls)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.stack.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MainTab {
    Controls,
    Dashboard,
    Health,
    Mcp,
    Memory,
    Projects,
    Tools,
    Settings,
    Data,
    Logs,
}

impl MainTab {
    /// Whether this tab already owns its scrolling.
    ///
    /// Tools wraps its whole body in a scroll area and Logs drives a bounded
    /// log viewport; nesting those inside an outer scroll area makes the inner
    /// one grow to its content instead of scrolling. Every other tab laid its
    /// content straight into the panel and simply ran off the bottom.
    pub(super) fn scrolls_itself(self) -> bool {
        matches!(self, Self::Tools | Self::Logs)
    }

    /// Title shown in the navigation bar.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Controls => "Solo Controls",
            Self::Dashboard => "Dashboard",
            Self::Health => "Health",
            Self::Mcp => "MCP Status",
            Self::Memory => "Memory",
            Self::Projects => "Projects",
            Self::Tools => "Connected Tools",
            Self::Settings => "Settings",
            Self::Data => "Data",
            Self::Logs => "Logs",
        }
    }
}
