use crate::protocol::{HookEvent, HookEventKind, SessionPhase, WorkerKind};

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HookEffect {
    Reset,
    Working {
        tool_name: Option<String>,
    },
    Waiting {
        tool_name: Option<String>,
        reason: String,
    },
    Done,
    RecordOnly,
}

pub(crate) fn blocked_reason(worker: WorkerKind, screen: &str) -> Option<&'static str> {
    match worker {
        WorkerKind::Grok => {
            if screen.contains("Run Grok Build in a project directory?") {
                Some("grok-project-directory")
            } else if screen.contains("Type your answer here") || screen.contains("Enter:submit") {
                Some("grok-interactive-prompt")
            } else {
                None
            }
        }
        WorkerKind::Codex => {
            if screen.contains("Trust this directory")
                || screen.contains("Do you trust the contents of this directory?")
                || screen.contains("trust the files in this folder")
                || screen.contains("Trust this folder?")
                || screen.contains("Trust and continue")
            {
                Some("codex-trust-directory")
            } else if screen.contains("trust these hooks")
                || screen.contains("untrusted hook")
                || screen.contains("Allow this hook")
                || screen.contains("Hooks need review")
                || screen.contains("Trust all and continue")
            {
                Some("codex-hook-trust")
            } else if screen.contains("Yes, proceed")
                || screen.contains("Allow command")
                || screen.contains("Approve command")
                || screen.contains("Press enter to confirm")
            {
                Some("codex-approval-prompt")
            } else {
                None
            }
        }
    }
}

pub(crate) fn phase_from_title(worker: WorkerKind, title: Option<&str>) -> Option<SessionPhase> {
    let title = title?.trim();
    let lower = title.to_ascii_lowercase();
    let suffix = match worker {
        WorkerKind::Grok => "grok",
        WorkerKind::Codex => "codex",
    };
    let hyphen_suffix = format!(" - {suffix}");
    if title_has_braille_spinner(title)
        && (lower.ends_with(suffix) || lower.contains(&hyphen_suffix))
    {
        return Some(SessionPhase::Running);
    }
    if lower == suffix || lower.ends_with(&hyphen_suffix) {
        return Some(SessionPhase::Idle);
    }
    if worker == WorkerKind::Codex {
        if title_has_braille_spinner(title) {
            return Some(SessionPhase::Running);
        }
        if lower == "ready" || lower.starts_with("ready |") {
            return Some(SessionPhase::Idle);
        }
    }
    None
}

fn title_has_braille_spinner(title: &str) -> bool {
    title
        .chars()
        .next()
        .is_some_and(|character| ('\u{2800}'..='\u{28ff}').contains(&character))
}

pub(crate) fn hook_effect(event: &HookEvent) -> HookEffect {
    match event.kind {
        HookEventKind::SessionStart => HookEffect::Reset,
        HookEventKind::UserPromptSubmit => HookEffect::Working {
            tool_name: event.tool_name.clone(),
        },
        HookEventKind::PostToolUse
        | HookEventKind::PostToolUseFailure
        | HookEventKind::PermissionDenied
        | HookEventKind::PreCompact
        | HookEventKind::PostCompact => HookEffect::Working { tool_name: None },
        HookEventKind::PermissionRequest => HookEffect::Waiting {
            tool_name: event.tool_name.clone(),
            reason: event
                .message
                .clone()
                .or_else(|| event.tool_name.clone())
                .unwrap_or_else(|| "permission_request".to_owned()),
        },
        HookEventKind::PreToolUse if is_ask_user_question(event.tool_name.as_deref()) => {
            HookEffect::Waiting {
                tool_name: event.tool_name.clone(),
                reason: event
                    .message
                    .clone()
                    .unwrap_or_else(|| "ask_user_question".to_owned()),
            }
        }
        HookEventKind::PreToolUse => HookEffect::Working {
            tool_name: event.tool_name.clone(),
        },
        HookEventKind::Stop
        | HookEventKind::StopFailure
        | HookEventKind::SessionEnd
        | HookEventKind::Interrupt => HookEffect::Done,
        HookEventKind::Notification => notification_effect(event),
        HookEventKind::SubagentStart | HookEventKind::SubagentStop => HookEffect::RecordOnly,
    }
}

fn is_ask_user_question(tool_name: Option<&str>) -> bool {
    tool_name.is_some_and(|name| name.eq_ignore_ascii_case("ask_user_question"))
}

fn notification_effect(event: &HookEvent) -> HookEffect {
    let notification_type = event
        .notification_type
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if notification_type == "permission_prompt" {
        return HookEffect::RecordOnly;
    }

    let level = event
        .level
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let message = event.message.as_deref().unwrap_or_default();
    let lower_message = message.to_ascii_lowercase();
    let waiting = [
        "permission",
        "question",
        "ask_user_question",
        "elicitation",
        "elicitation_dialog",
    ]
    .iter()
    .any(|value| notification_type == *value || level == *value)
        || ["permission", "approval", "approve", "question"]
            .iter()
            .any(|value| lower_message.contains(value))
        || ["权限", "授权", "批准", "问题", "确认"]
            .iter()
            .any(|value| message.contains(value));
    if waiting {
        let reason = event
            .message
            .clone()
            .or_else(|| event.notification_type.clone())
            .unwrap_or_else(|| "agent-hook-waiting".to_owned());
        return HookEffect::Waiting {
            tool_name: event.tool_name.clone(),
            reason,
        };
    }

    let done = [
        "idle_prompt",
        "input_prompt",
        "input_required",
        "user_input",
        "waiting_for_input",
    ]
    .iter()
    .any(|value| notification_type == *value || level == *value)
        || ["waiting for input", "waiting for your input"]
            .iter()
            .any(|value| lower_message.contains(value))
        || ["请输入", "等待输入", "需要输入"]
            .iter()
            .any(|value| message.contains(value));
    if done {
        HookEffect::Done
    } else {
        HookEffect::RecordOnly
    }
}
