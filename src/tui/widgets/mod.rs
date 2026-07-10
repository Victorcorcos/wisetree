//! Reusable TUI primitives. Stateful widgets (input/select/confirm) own
//! their state and expose a `handle_key` method returning an outcome enum;
//! purely presentational widgets (spinner / status / command-list) are
//! drawn directly from the caller's state.

pub mod border;
pub mod bulk_confirm_dialog;
pub mod command_list_progress;
pub mod command_progress;
pub mod confirmation_modal;
pub mod input_prompt;
pub mod pty_view;
pub mod run_transcript;
pub mod scrollbar;
pub mod select_prompt;
pub mod spinner;
pub mod status_indicator;
pub mod summary_table;
pub mod toast;
pub mod update_banner;
pub mod welcome_header;

pub use border::BorderState;
pub use bulk_confirm_dialog::{
    BulkConfirmDialog, BulkConfirmFocus, BulkConfirmItem, BulkConfirmOutcome, ConfirmVariant,
};
pub use command_list_progress::CommandListProgress;
pub use command_progress::CommandProgress;
pub use confirmation_modal::{ConfirmationChoice, ConfirmationModal, ConfirmationOutcome};
pub use input_prompt::{InputOutcome, InputPrompt};
pub use pty_view::PtyView;
pub use run_transcript::RunTranscriptView;
pub use scrollbar::render_vertical_scrollbar;
pub use select_prompt::{
    branded_line, branded_spans, SelectOption, SelectOutcome, SelectPrompt, SelectStyle,
    SELECT_CURSOR,
};
pub use spinner::{spinner_frame, Spinner, SPINNER_FRAMES};
pub use status_indicator::{Status, StatusIndicator};
pub use summary_table::{render_summary_table, RowStatus, SummaryRow};
pub use toast::{render_toast, ToastSnapshot, ToastState, ToastVariant};
pub use update_banner::UpdateBanner;
pub use welcome_header::WelcomeHeader;
