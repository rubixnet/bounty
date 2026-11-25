//! `/stop-track` and `/resume-track`: the Session Pause commands.
//!
//! The hook endpoint and the installer share these definitions: the
//! endpoint matches a `UserPromptSubmit` prompt against [`Command::literal`]
//! and [`Command::body`], the installer writes [`Command::file_name`] with
//! [`Command::description`] and that same body. The body names the Traction
//! action and gives the model no instructions (Zero LLM Trust Boundary).

/// One Session Pause command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// What pausing does to the session.
    pub action: PauseAction,
    /// The text the user types, and what Claude Code hands the hook.
    pub literal: &'static str,
    /// The command file in `<claude config dir>/commands/`.
    pub file_name: &'static str,
    /// The frontmatter `description` Claude Code shows in its command list.
    pub description: &'static str,
    /// The one-line body after the frontmatter. Matched too, in case a
    /// Claude Code version hands the hook the expanded command.
    pub body: &'static str,
    /// The `reason` of the block answer, shown to the user.
    pub reason: &'static str,
}

/// Whether a command pauses or resumes its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    Stop,
    Resume,
}

pub const STOP_TRACK: Command = Command {
    action: PauseAction::Stop,
    literal: "/stop-track",
    file_name: "stop-track.md",
    description: "Pause Traction tracking for this session",
    body: "Traction: pause tracking for this session.",
    reason: "Traction tracking paused for this session.",
};

pub const RESUME_TRACK: Command = Command {
    action: PauseAction::Resume,
    literal: "/resume-track",
    file_name: "resume-track.md",
    description: "Resume Traction tracking for this session",
    body: "Traction: resume tracking for this session.",
    reason: "Traction tracking resumed for this session.",
};

/// The block `reason` when the command could not be queued (the Router is
/// full): the prompt is still kept from the model, and nothing changed.
pub const BUSY_REASON: &str = "Traction is busy and did not change tracking. Try again.";

impl Command {
    pub const ALL: [Command; 2] = [STOP_TRACK, RESUME_TRACK];

    /// The command `prompt` is, exactly: trimmed of surrounding whitespace,
    /// it equals a command's literal or its body. Anything else (including
    /// a prompt that merely mentions a command) is `None`.
    pub fn matching(prompt: &str) -> Option<Command> {
        let prompt = prompt.trim();
        Command::ALL
            .into_iter()
            .find(|command| prompt == command.literal || prompt == command.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_bodies_are_distinct_and_trimmed() {
        for command in Command::ALL {
            assert_eq!(command.body, command.body.trim());
            assert_eq!(Command::matching(command.literal), Some(command));
            assert_eq!(Command::matching(command.body), Some(command));
        }
        assert_ne!(STOP_TRACK.body, RESUME_TRACK.body);
    }
}
