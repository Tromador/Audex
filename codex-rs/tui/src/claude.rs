//! Parse leading route options only; the message remains literal conversation text.
pub(crate) const USAGE: &str = "Usage: /claude [--model MODEL] [--audit RUN_ID | --no-audit] <message> (uses the active audit when selected; --audit overrides it for this turn, --no-audit uses ordinary conversation)";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub model: Option<String>,
    pub audit: Option<String>,
    pub no_audit: bool,
    pub message: String,
}

pub(crate) fn parse(args: &str) -> Result<Request, String> {
    let mut message = args.trim();
    let mut model = None;
    let mut audit = None;
    let mut no_audit = false;
    loop {
        let end = message.find(char::is_whitespace).unwrap_or(message.len());
        let token = &message[..end];
        let (flag, inline) = token
            .split_once('=')
            .map_or((token, None), |(k, v)| (k, Some(v)));
        if flag == "--no-audit" {
            if inline.is_some() || no_audit || audit.is_some() {
                return Err(USAGE.into());
            }
            no_audit = true;
            message = message[end..].trim_start();
            continue;
        }
        if flag == "--audit" && no_audit {
            return Err(USAGE.into());
        }
        let target = match flag {
            "--model" => &mut model,
            "--audit" => &mut audit,
            _ => break,
        };
        if target.is_some() {
            return Err(USAGE.into());
        }
        message = message[end..].trim_start();
        let value = if let Some(value) = inline {
            value
        } else {
            let end = message.find(char::is_whitespace).unwrap_or(message.len());
            let value = &message[..end];
            message = message[end..].trim_start();
            value
        };
        if value.is_empty() || value.starts_with('-') {
            return Err(USAGE.into());
        }
        *target = Some(value.to_owned());
    }
    if message.is_empty() {
        return Err(USAGE.into());
    }
    Ok(Request {
        model,
        audit,
        no_audit,
        message: message.to_owned(),
    })
}

pub(crate) fn audit_context(
    explicit: Option<&str>,
    no_audit: bool,
    active: Option<&str>,
) -> Option<String> {
    if no_audit {
        None
    } else {
        explicit.or(active).map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_context_default_model_override_explicit_and_opt_out() {
        for (args, active, expected) in [
            ("question", Some("a"), Some("a")),
            ("--model chosen question", Some("a"), Some("a")),
            ("question", Some("b"), Some("b")),
            ("--audit c question", Some("a"), Some("c")),
            ("--no-audit question", Some("a"), None),
            ("question", None, None),
        ] {
            let request = parse(args).unwrap();
            assert_eq!(
                audit_context(request.audit.as_deref(), request.no_audit, active).as_deref(),
                expected
            );
            assert_eq!(request.message, "question");
            if args.starts_with("--model") {
                assert_eq!(request.model.as_deref(), Some("chosen"));
            }
        }
        for args in [
            "--audit a --no-audit question",
            "--no-audit --audit a question",
            "--no-audit --no-audit question",
            "--no-audit=x question",
        ] {
            assert!(parse(args).is_err(), "{args}");
        }
    }
    #[test]
    fn retains_literal_message_and_model() {
        let r = parse("Isn't Codex's answer wrong?\n  Check `x`.").unwrap();
        assert_eq!(r.message, "Isn't Codex's answer wrong?\n  Check `x`.");
        assert_eq!(r.model, None);
        assert_eq!(r.audit, None);
        let r = parse("--model claude-sonnet-5-5 a  b\n--flag 'x'").unwrap();
        assert_eq!(r.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(r.message, "a  b\n--flag 'x'");
        for args in [
            "",
            "--model",
            "--model x",
            "--model= hello",
            "--audit",
            "--audit= x",
            "--audit a --audit b x",
        ] {
            assert!(parse(args).is_err(), "{args}");
        }
    }
    #[test]
    fn usage_describes_active_context_and_explicit_turn_overrides() {
        assert_eq!(
            parse("--audit run-a inspect finding")
                .unwrap()
                .audit
                .as_deref(),
            Some("run-a")
        );
        assert_eq!(parse("continue the discussion").unwrap().audit, None);
        let usage = parse("").unwrap_err();
        assert!(usage.contains("uses the active audit when selected"));
        assert!(!usage.contains("continues in this thread"));
    }

    #[test]
    fn explicit_audit_and_model_options_in_either_order() {
        for args in [
            "--audit run-a --model=claude-sonnet-5-5 check --audit literal",
            "--model claude-sonnet-5-5 --audit=run-a check --audit literal",
        ] {
            assert_eq!(
                parse(args).unwrap(),
                Request {
                    model: Some("claude-sonnet-5-5".into()),
                    audit: Some("run-a".into()),
                    no_audit: false,
                    message: "check --audit literal".into()
                }
            );
        }
    }
}
