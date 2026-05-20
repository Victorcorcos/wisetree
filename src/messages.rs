//! User-facing strings ported verbatim from the upstream TS catalog.
//!
//! These literals are part of the user-visible UX contract — keep them in
//! sync with the strings displayed in screens and tests.

// Welcome and general
pub const WELCOME: &str = "Wisetree - Git Worktree Manager";

// Menu options
pub const MENU_TITLE: &str = "Choose wisely...";
pub const MENU_SETUP_PROJECT: &str = "Setup Project Config";
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

    // ── Background colors ───────────────────────────────────────────────

    /// Brown darker `#282922` — main app background.
    pub const BG: Color = Color::Rgb(0x28, 0x29, 0x22);
    /// Brown lighter `#3e3d31` — selected row / status bar background.
    pub const BG_SELECTED: Color = Color::Rgb(0x3e, 0x3d, 0x31);
    /// Brown even lighter `#75705b` — focus background for elements that
    /// need to stand out without leaving the brown family.
    pub const BG_FOCUS: Color = Color::Rgb(0x75, 0x70, 0x5b);

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

    /// Monokai-flavoured aliases reused by the AI Activity panel so its
    /// syntax-highlighted fenced code blocks stay aligned with the rest
    /// of the palette without hard-coding raw RGB tuples at the call
    /// site.
    pub mod monokai {
        use super::opencode;
        use ratatui::style::Color;

        pub const SYNTAX_KEYWORD: Color = opencode::PINK;
        pub const SYNTAX_FUNCTION: Color = opencode::GREEN;
        pub const SYNTAX_STRING: Color = opencode::YELLOW;
        pub const SYNTAX_NUMBER: Color = opencode::PURPLE;
        pub const SYNTAX_TYPE: Color = opencode::CYAN;
        pub const SYNTAX_COMMENT: Color = opencode::COMMENT;
    }

    /// Purple `#ae81ff` — Monokai number literal accent. Re-exported as
    /// `PURPLE_FUNCTION` so the AI Activity palette can pull it in
    /// without shadowing the existing `BRAND` purple.
    pub const PURPLE_FUNCTION: Color = opencode::PURPLE;

    /// Opencode Monokai palette — the source of truth for the AI Activity
    /// panel. Values are kept in sync with `design/opencode.md` and
    /// `packages/opencode/.../monokai.json` so wisetree's transcript reads
    /// identically to what opencode prints in its own TUI. Use these
    /// constants for anything rendered inside the AI Activity surface; the
    /// rest of the app keeps using the wisetree palette declared above.
    pub mod opencode {
        use ratatui::style::Color;

        // ── Base tokens (`defs`) ───────────────────────────────────────

        /// `#272822` — main panel background.
        pub const BG: Color = Color::Rgb(0x27, 0x28, 0x22);
        /// `#1e1f1c` — alternate / code-block backdrop.
        pub const BG_ALT: Color = Color::Rgb(0x1e, 0x1f, 0x1c);
        /// `#3e3d32` — elevated surface (selected row, status strip).
        pub const BG_PANEL: Color = Color::Rgb(0x3e, 0x3d, 0x32);
        /// `#f8f8f2` — foreground text.
        pub const FG: Color = Color::Rgb(0xf8, 0xf8, 0xf2);
        /// `#75715e` — comment / muted text.
        pub const COMMENT: Color = Color::Rgb(0x75, 0x71, 0x5e);
        /// `#f92672` — pink/red accent (errors, keywords, headings).
        pub const PINK: Color = Color::Rgb(0xf9, 0x26, 0x72);
        /// `#fd971f` — orange accent (info, **bold**).
        pub const ORANGE: Color = Color::Rgb(0xfd, 0x97, 0x1f);
        /// `#e6db74` — yellow accent (warnings, strings, *italic*).
        pub const YELLOW: Color = Color::Rgb(0xe6, 0xdb, 0x74);
        /// `#a6e22e` — green accent (success, functions, inline `code`,
        /// diff additions).
        pub const GREEN: Color = Color::Rgb(0xa6, 0xe2, 0x2e);
        /// `#66d9ef` — cyan / primary accent (borders, types, list
        /// bullets, links).
        pub const CYAN: Color = Color::Rgb(0x66, 0xd9, 0xef);
        /// `#ae81ff` — purple accent (numbers, link text, enumerations).
        pub const PURPLE: Color = Color::Rgb(0xae, 0x81, 0xff);
        /// `#9b9b95` — diff line-number gutter color.
        pub const DIFF_LINE_NUMBER: Color = Color::Rgb(0x9b, 0x9b, 0x95);
        /// `#1a3a1a` — diff added line background.
        pub const DIFF_ADDED_BG: Color = Color::Rgb(0x1a, 0x3a, 0x1a);
        /// `#3a1a1a` — diff removed line background.
        pub const DIFF_REMOVED_BG: Color = Color::Rgb(0x3a, 0x1a, 0x1a);

        // ── Semantic aliases ───────────────────────────────────────────

        pub const TEXT: Color = FG;
        pub const TEXT_MUTED: Color = COMMENT;
        pub const PRIMARY: Color = CYAN;
        pub const SECONDARY: Color = PURPLE;
        pub const ACCENT: Color = GREEN;
        pub const SUCCESS: Color = GREEN;
        pub const INFO: Color = ORANGE;
        pub const WARNING: Color = YELLOW;
        pub const ERROR: Color = PINK;
        pub const BORDER: Color = BG_PANEL;
        pub const BORDER_ACTIVE: Color = CYAN;

        // ── Markdown roles ─────────────────────────────────────────────

        pub const MD_TEXT: Color = FG;
        pub const MD_HEADING: Color = PINK;
        pub const MD_STRONG: Color = ORANGE;
        pub const MD_EMPH: Color = YELLOW;
        pub const MD_CODE: Color = GREEN;
        pub const MD_CODE_BLOCK: Color = FG;
        pub const MD_LINK: Color = CYAN;
        pub const MD_LINK_TEXT: Color = PURPLE;
        pub const MD_LIST_ITEM: Color = CYAN;
        pub const MD_LIST_ENUM: Color = PURPLE;
        pub const MD_BLOCK_QUOTE: Color = COMMENT;
        pub const MD_HORIZONTAL_RULE: Color = COMMENT;

        // ── Syntax roles ───────────────────────────────────────────────

        pub const SYNTAX_COMMENT: Color = COMMENT;
        pub const SYNTAX_KEYWORD: Color = PINK;
        pub const SYNTAX_FUNCTION: Color = GREEN;
        pub const SYNTAX_VARIABLE: Color = FG;
        pub const SYNTAX_STRING: Color = YELLOW;
        pub const SYNTAX_NUMBER: Color = PURPLE;
        pub const SYNTAX_TYPE: Color = CYAN;
        pub const SYNTAX_OPERATOR: Color = PINK;
        pub const SYNTAX_PUNCTUATION: Color = FG;

        // ── Diff roles ─────────────────────────────────────────────────

        pub const DIFF_ADDED: Color = GREEN;
        pub const DIFF_REMOVED: Color = PINK;
        pub const DIFF_CONTEXT: Color = COMMENT;
        pub const DIFF_HUNK_HEADER: Color = COMMENT;
    }
}
