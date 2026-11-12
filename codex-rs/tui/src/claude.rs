//! Parse leading route options only; the message remains literal conversation text.
pub(crate) const USAGE: &str = "Usage: /claude [--model MODEL] [--audit RUN_ID] <message> (default model: configured Claude model, initially claude-haiku-5-5; audit selection continues in this thread)";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub model: Option<String>,
    pub audit: Option<String>,
    pub message: String,
}

pub(crate) fn parse(args: &str) -> Result<Request, String> {
    let mut message = args.trim();
    let mut model = None;
    let mut audit = None;
    loop {
        let end = message.find(char::is_whitespace).unwrap_or(message.len());
        let token = &message[..end];
        let (flag, inline) = token
            .split_once('=')
            .map_or((token, None), |(k, v)| (k, Some(v)));
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
        message: message.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
                    message: "check --audit literal".into()
                }
            );
        }
    }
}
