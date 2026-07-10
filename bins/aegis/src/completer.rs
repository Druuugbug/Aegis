//! Slash-command registry — the single source of truth for available commands.
//!
//! [`SLASH_COMMANDS`] is consumed by:
//! - `reedline_input.rs` (IDE-style completion menu)
//! - `chat.rs` (`/help` renderer)
//! - `gateway.rs` (web UI autocomplete)

/// `(command, description)`. A trailing space on the command means it takes an
/// argument (completion leaves the cursor ready to type it).
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this help"),
    ("/new", "start a new session (keeps long-term memory)"),
    ("/attach ", "attach a file (image/pdf) to next message: /attach path/to/file"),
    ("/undo", "undo the last turn"),
    ("/retry", "re-run the last message"),
    ("/memory", "memories: /memory <query> | --all | add <text> | restore <id>"),
    ("/forget ", "delete a stored memory by id"),
    ("/secret", "secrets: /secret add <name> <value> | list | reveal <name> | remove <name>"),
    ("/queue", "queued instructions: /queue | /queue remove <n> | /queue clear (type while busy to queue)"),
    ("/stop", "stop the task aegis is currently running (or press Ctrl+C)"),
    ("/expand", "show the full output of the last tool (preview is collapsed); alias /o"),
    ("/thinking", "show the model's reasoning from the last turn (normally folded)"),
    ("/profile", "show what aegis has learned about you"),
    ("/style ", "answer verbosity: normal | concise | minimal"),
    ("/set ", "adjust a setting (e.g. /set components.tier advanced)"),
    ("/steer add ", "add a steering instruction (permanent)"),
    ("/steer add-n ", "add a steering instruction for N turns"),
    ("/steer list", "list steering instructions"),
    ("/steer remove ", "remove a steering instruction by id"),
    ("/steer clear", "clear all steering instructions"),
    ("/search ", "search past sessions"),
    ("/resume", "list past sessions; /resume <n|id> loads one as background"),
    ("/server ", "manage remote servers locally: add <name> <host> <user> [pw] | list | remove"),
    ("/history", "show conversation history"),
    ("/save", "export this session to JSON"),
    ("/config", "show model, session and token usage"),
    ("/usage", "token usage: this session, or history: /usage today|week|month|all [by-day|by-model]"),
    ("/verbose", "toggle verbose output"),
    ("/rollback", "restore a previous checkpoint"),
    ("/setup", "open the setup wizard"),
    ("/quit", "exit (also /exit)"),
];
