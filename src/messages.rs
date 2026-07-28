//! User-facing strings ported verbatim from the upstream TS catalog.
//!
//! These literals are part of the user-visible UX contract — keep them in
//! sync with the strings displayed in screens and tests.

// Welcome and general
pub const WELCOME: &str = "Wisetree - Git Worktree Manager";

// Menu options
pub const MENU_TITLE: &str = "Choose wisely...";
pub const MENU_SETUP: &str = "Setup Shell Integration";
pub const MENU_CREATE: &str = "Create";
pub const MENU_DASHBOARD: &str = "Dashboard";
pub const MENU_CACHE: &str = "Shared cache";
pub const MENU_SETTINGS: &str = "Settings";
pub const MENU_EXIT: &str = "Exit";

// Create flow
pub const CREATE_DIRECTORY_PROMPT: &str = "Enter directory name for the new worktree:";
pub const CREATE_DIRECTORY_PLACEHOLDER: &str = "worktree-name";
pub const CREATE_SOURCE_BRANCH_PROMPT: &str = "Select source branch:";
pub const CREATE_NEW_BRANCH_PROMPT: &str =
    "Enter name for new branch (leave blank to use source branch):";
pub const CREATE_NEW_BRANCH_PLACEHOLDER: &str = "feat/new-feature or leave blank";
pub const CREATE_CONFIRM_TITLE: &str = "Create Worktree Confirmation";
pub const CREATE_NAVIGATE_TITLE: &str = "Navigate to Worktree";
pub const CREATE_NAVIGATE_PROMPT: &str =
    "Navigate directly into the created worktree once it is ready?";
pub const CREATE_SUCCESS: &str = "Worktree created successfully!";
pub const CREATE_CREATING: &str = "Creating worktree...";

// Delete flow
pub const DELETE_CONFIRM_TITLE: &str = "Delete Worktree Confirmation";
pub const DELETE_WARNING: &str = "This action cannot be undone.";
pub const DELETE_SUCCESS: &str = "Worktree deleted successfully!";
pub const DELETE_DELETING: &str = "Deleting worktree...";

// Validation errors
pub const ERROR_NOT_GIT_REPO: &str = "Current directory is not a git repository.";
pub const ERROR_DIRECTORY_EXISTS: &str = "Directory already exists.";
pub const ERROR_INVALID_DIRECTORY_NAME: &str = "Invalid directory name.";
pub const ERROR_INVALID_BRANCH_NAME: &str = "Invalid branch name.";
pub const ERROR_BRANCH_EXISTS: &str = "Branch already exists.";
pub const ERROR_WORKTREE_EXISTS: &str = "Worktree already exists.";
pub const ERROR_WORKTREE_HAS_CHANGES: &str = "Worktree has uncommitted changes.";
pub const ERROR_OPERATION_FAILED: &str = "Operation failed. Please try again.";
pub const ERROR_AI_BINARY_MISSING: &str = "The selected AI CLI is not installed.";
pub const ERROR_AI_AUTH_MISSING: &str = "The selected AI CLI is not authenticated.";
pub const ERROR_AI_MODEL_UNAVAILABLE: &str = "The selected AI model is unavailable.";
pub const ERROR_AI_EFFORT_UNSUPPORTED: &str = "The selected AI effort is not supported.";
pub const ERROR_AI_FLAGS_UNSUPPORTED: &str =
    "The selected AI CLI does not support the required flags.";

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

/// Monokai-inspired palette defined in `design/pallete.md`. Every visible
/// color in the TUI must resolve to one of these constants — see that file
/// for the recommended usage of each.
pub mod colors {
    use ratatui::style::Color;

    // ── Brand & font colors ─────────────────────────────────────────────

    /// Purple `#b47eff` — reserved for the words `wisetree`, `worktree`,
    /// and `worktrees` to anchor the brand identity.
    pub const BRAND: Color = Color::Rgb(0xb4, 0x7e, 0xff);
    /// White `#f8f8f1` — main font color for primary content.
    pub const WHITE: Color = Color::Rgb(0xf8, 0xf8, 0xf1);
    /// Gray darker `#90918a` — annotation text such as
    /// `Version 1.0.0 | Active Repo:`.
    pub const GRAY_DARK: Color = Color::Rgb(0x90, 0x91, 0x8a);
    /// Gray medium `#b4b5ae` — sits between [`GRAY_DARK`] and
    /// [`GRAY_LIGHT`]. Used for the `Drafted` PR status so it reads as
    /// distinct from `Closed` (which uses [`GRAY_LIGHT`]).
    pub const GRAY_MEDIUM: Color = Color::Rgb(0xb4, 0xb5, 0xae);
    /// Gray lighter `#d9d9d2` — emphasized annotation text such as the
    /// active repository path.
    pub const GRAY_LIGHT: Color = Color::Rgb(0xd9, 0xd9, 0xd2);
    /// Pink `#ff0071` — error messages and destructive states.
    pub const PINK: Color = Color::Rgb(0xff, 0x00, 0x71);
    /// Green `#94e400` — success messages and positive states.
    pub const GREEN: Color = Color::Rgb(0x94, 0xe4, 0x00);
    /// Teal `#1cdbf2` — informational text and titles like
    /// `Choose wisely...`.
    pub const TEAL: Color = Color::Rgb(0x1c, 0xdb, 0xf2);
    /// Yellow `#eada61` — warnings such as
    /// `Directory name cannot be empty`.
    pub const YELLOW: Color = Color::Rgb(0xea, 0xda, 0x61);
    /// Orange `#ff8f00` — creative accent, used sparingly for highlights
    /// like progress headers and "running" states.
    pub const ORANGE: Color = Color::Rgb(0xff, 0x8f, 0x00);
    /// Cyan `#a8d8ff` — soft blue accent from the extended palette. Used for
    /// the "Fix" PR command button so it reads distinct from the teal/purple
    /// lifecycle buttons next to it.
    pub const CYAN: Color = Color::Rgb(0xa8, 0xd8, 0xff);
    /// Dark green `#58a300` — darker than [`GREEN`]. Used for the "Bugkill"
    /// PR command button so it reads distinct from the Merge button's
    /// success green next to it.
    pub const DARK_GREEN: Color = Color::Rgb(0x58, 0xa3, 0x00);
    /// Navy blue `#4d7cfe` — deeper than [`CYAN`]. Used for the "Review"
    /// PR command button (and its confirm screen) so it reads distinct from
    /// the soft-blue Fix button next to it while staying legible on the
    /// brown background.
    pub const NAVY: Color = Color::Rgb(0x4d, 0x7c, 0xfe);
    /// Teal accent reserved for the local "Improve" command and its
    /// confirmation screen.
    pub const IMPROVE: Color = TEAL;

    // ── Background colors ───────────────────────────────────────────────

    /// Brown darker `#282922` — main app background.
    pub const BG: Color = Color::Rgb(0x28, 0x29, 0x22);
    /// Brown lighter `#3e3d31` — selected row / status bar background.
    pub const BG_SELECTED: Color = Color::Rgb(0x3e, 0x3d, 0x31);
    /// Brown even lighter `#75705b` — focus background for elements that
    /// need to stand out without leaving the brown family.
    pub const BG_FOCUS: Color = Color::Rgb(0x75, 0x70, 0x5b);
    /// Neutral gray `#4a4a4a` — deliberately breaks from the brown family so
    /// inline code spans (`` `like this` ``) read as a distinct "code chip"
    /// rather than another shade of the surrounding UI.
    pub const CODE_BG: Color = Color::Rgb(0x4a, 0x4a, 0x4a);

    // ── Semantic aliases ────────────────────────────────────────────────
    //
    // These names describe *intent*, not color. Map them to the palette
    // above so call sites stay readable. Adjust the alias here if you ever
    // want to retune a role without touching every screen.

    /// Primary accent (spinners, selected rows, install commands).
    pub const PRIMARY: Color = TEAL;
    /// Success state.
    pub const SUCCESS: Color = GREEN;
    /// Warning state.
    pub const WARNING: Color = YELLOW;
    /// Error state.
    pub const ERROR: Color = PINK;
    /// Informational text and headings.
    pub const INFO: Color = TEAL;
    /// Muted / secondary annotation text.
    pub const MUTED: Color = GRAY_DARK;
    /// Emphasized annotation text (paths, important values).
    pub const EMPHASIS: Color = GRAY_LIGHT;
    /// Brand accent for the words `Wisetree` / `worktree`.
    pub const HIGHLIGHT: Color = BRAND;
    /// Creative accent for moments worth a splash of color.
    pub const ACCENT: Color = ORANGE;

    // ── App / panel surfaces ───────────────────────────────────────────

    /// Application background.
    pub const APP_BG: Color = BG;
    /// Welcome header backdrop.
    pub const HEADER_BG: Color = BG;
    /// Welcome header border — uses the focus brown so the panel frame
    /// reads as scaffolding rather than competing with the brand color
    /// on the title text inside it.
    pub const HEADER_BORDER: Color = BG_FOCUS;
    /// Welcome header title text.
    pub const HEADER_TITLE: Color = WHITE;
    /// Welcome header annotation text (labels like `Current Repository`).
    pub const HEADER_SUBTITLE: Color = GRAY_DARK;
    /// Menu panel backdrop.
    pub const MENU_BG: Color = BG;
    /// Menu panel border — uses the focus brown so the frame stays in the
    /// brown family while the teal accent is reserved for the title text.
    pub const MENU_BORDER: Color = BG_FOCUS;
    /// Selected menu row background.
    pub const MENU_SELECTION_BG: Color = BG_SELECTED;
    /// Selected menu row foreground.
    pub const MENU_SELECTION_FG: Color = WHITE;
    /// Menu body text.
    pub const MENU_TEXT: Color = GRAY_LIGHT;
    /// Status bar backdrop.
    pub const STATUS_BG: Color = BG_SELECTED;
    /// Status bar foreground.
    pub const STATUS_TEXT: Color = GRAY_LIGHT;

    // ── AI Status column ───────────────────────────────────────────────
    //
    // Identity colors for the per-harness decoration letters in the new
    // AI Status column. Each harness keeps a stable color regardless of
    // its current state so the eye can pick out which harness is which
    // even when the row is dim. See PLAN.md §2.

    /// `C` — Claude Code identity color.
    pub const HARNESS_CLAUDE: Color = Color::Rgb(0xe0, 0x7a, 0x5f);
    /// `O` — Opencode identity color.
    pub const HARNESS_OPENCODE: Color = Color::Rgb(0x9d, 0x7c, 0xd8);
    /// `X` — Codex identity color (intentionally the same gray as `MUTED`).
    pub const HARNESS_CODEX: Color = GRAY_DARK;
    /// `G` — Gemini identity color.
    pub const HARNESS_GEMINI: Color = Color::Rgb(0xe8, 0x79, 0xa6);

    // ── Diff bars (Fix → Proposed fix) ──────────────────────────────────
    //
    // Used to render a ```diff block the way GitHub does: a full-width
    // colored bar behind each added / removed line. Backgrounds are dark
    // tints that sit on the brown app surface; foregrounds stay legible on
    // top of them.

    /// Added line (`+`) foreground.
    pub const DIFF_ADD_FG: Color = Color::Rgb(0xc3, 0xe8, 0x8a);
    /// Added line (`+`) background — dark green tint.
    pub const DIFF_ADD_BG: Color = Color::Rgb(0x20, 0x33, 0x16);
    /// Removed line (`-`) foreground.
    pub const DIFF_REMOVE_FG: Color = Color::Rgb(0xf2, 0xa3, 0xb6);
    /// Removed line (`-`) background — dark red tint.
    pub const DIFF_REMOVE_BG: Color = Color::Rgb(0x3a, 0x18, 0x20);
    /// Hunk header (`@@ … @@`) foreground — no background.
    pub const DIFF_HUNK_FG: Color = Color::Rgb(0xb4, 0x7e, 0xff);
}
