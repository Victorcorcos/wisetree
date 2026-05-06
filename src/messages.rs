//! User-facing strings ported verbatim from the upstream TS catalog.
//!
//! These literals are part of the user-visible UX contract — keep them in
//! sync with the strings displayed in screens and tests.

// Welcome and general
pub const WELCOME: &str = "Wisetree - Git Worktree Manager";

// Menu options
pub const MENU_TITLE: &str = "What would you like to do?";
pub const MENU_SETUP: &str = "Setup Shell Integration";
pub const MENU_CREATE: &str = "Create new worktree";
pub const MENU_LIST: &str = "List worktrees";
pub const MENU_DELETE: &str = "Delete worktree";
pub const MENU_SETTINGS: &str = "Settings";
pub const MENU_EXIT: &str = "Exit";

// Create flow
pub const CREATE_DIRECTORY_PROMPT: &str = "Enter directory name for the new worktree:";
pub const CREATE_DIRECTORY_PLACEHOLDER: &str = "feature-name";
pub const CREATE_SOURCE_BRANCH_PROMPT: &str = "Select source branch:";
pub const CREATE_NEW_BRANCH_PROMPT: &str =
    "Enter name for new branch (leave blank to use source branch):";
pub const CREATE_NEW_BRANCH_PLACEHOLDER: &str = "feat/new-feature or leave blank";
pub const CREATE_CONFIRM_TITLE: &str = "Create Worktree Confirmation";
pub const CREATE_SUCCESS: &str = "Worktree created successfully!";
pub const CREATE_CREATING: &str = "Creating worktree...";

// Delete flow
pub const DELETE_SELECT_PROMPT: &str = "Select worktree to delete:";
pub const DELETE_CONFIRM_TITLE: &str = "Delete Worktree Confirmation";
pub const DELETE_WARNING: &str = "This action cannot be undone.";
pub const DELETE_SUCCESS: &str = "Worktree deleted successfully!";
pub const DELETE_DELETING: &str = "Deleting worktree...";

// List view
pub const LIST_TITLE: &str = "Git Worktrees";
pub const LIST_NO_WORKTREES: &str = "No additional worktrees found.";
pub const LIST_MAIN_INDICATOR: &str = "(main)";
pub const LIST_DIRTY_INDICATOR: &str = "(dirty)";

// Validation errors
pub const ERROR_NOT_GIT_REPO: &str = "Current directory is not a git repository.";
pub const ERROR_DIRECTORY_EXISTS: &str = "Directory already exists.";
pub const ERROR_INVALID_DIRECTORY_NAME: &str = "Invalid directory name.";
pub const ERROR_INVALID_BRANCH_NAME: &str = "Invalid branch name.";
pub const ERROR_BRANCH_EXISTS: &str = "Branch already exists.";
pub const ERROR_WORKTREE_EXISTS: &str = "Worktree already exists.";
pub const ERROR_WORKTREE_HAS_CHANGES: &str = "Worktree has uncommitted changes.";
pub const ERROR_OPERATION_FAILED: &str = "Operation failed. Please try again.";

// Git errors
pub const GIT_ERROR_FETCH: &str = "Failed to fetch git information.";
pub const GIT_ERROR_CREATE: &str = "Failed to create worktree.";
pub const GIT_ERROR_DELETE: &str = "Failed to delete worktree.";
pub const GIT_ERROR_LIST: &str = "Failed to list worktrees.";

// File operations
pub const FILES_COPYING: &str = "Copying files...";
pub const FILES_COPY_SUCCESS: &str = "Files copied successfully.";
pub const FILES_COPY_ERROR: &str = "Failed to copy some files.";

// Post-create actions
pub const POST_CREATE_RUNNING: &str = "Running post-create command...";
pub const POST_CREATE_SUCCESS: &str = "Post-create command completed.";
pub const POST_CREATE_ERROR: &str = "Post-create command failed.";

// Navigation hints
pub const HINT_ARROW_KEYS: &str = "Use ↑↓ arrow keys to navigate";
pub const HINT_ENTER_SELECT: &str = "Press Enter to select";
pub const HINT_ESC_CANCEL: &str = "Press Esc to cancel";
pub const HINT_CTRL_C_EXIT: &str = "Press Ctrl+C to exit";

// Loading states
pub const LOADING_GIT_INFO: &str = "Loading git information...";
pub const LOADING_BRANCHES: &str = "Loading branches...";
pub const LOADING_WORKTREES: &str = "Loading worktrees...";

// Update checking
pub const UPDATE_AVAILABLE: &str = "Update available";
pub const UPDATE_CHECK_MENU: &str = "Check for Updates";
pub const UPDATE_CHECKING: &str = "Checking for updates...";
pub const UPDATE_UP_TO_DATE: &str = "You're running the latest version";
pub const UPDATE_FAILED: &str = "Failed to check for updates";
pub const UPDATE_INSTALL_CMD: &str = "npm install -g wisetree";

/// Color palette ported from the upstream `COLORS` map.
pub mod colors {
    use ratatui::style::Color;

    /// `#61dafb`
    pub const PRIMARY: Color = Color::Rgb(0x61, 0xda, 0xfb);
    /// `#28a745`
    pub const SUCCESS: Color = Color::Rgb(0x28, 0xa7, 0x45);
    /// `#ffc107`
    pub const WARNING: Color = Color::Rgb(0xff, 0xc1, 0x07);
    /// `#dc3545`
    pub const ERROR: Color = Color::Rgb(0xdc, 0x35, 0x45);
    /// `#17a2b8`
    pub const INFO: Color = Color::Rgb(0x17, 0xa2, 0xb8);
    /// `#6c757d`
    pub const MUTED: Color = Color::Rgb(0x6c, 0x75, 0x7d);
    /// `#007bff`
    pub const HIGHLIGHT: Color = Color::Rgb(0x00, 0x7b, 0xff);
}
