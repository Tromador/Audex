//! Parse only the leading model selection; the message remains literal conversation text.
pub(crate) const USAGE: &str = "Usage: /claude [--model MODEL] <message> (default: configured Claude model, initially claude-haiku-5-5)";

pub(crate) fn parse(args: &str) -> Result<(Option<String>, String), String> {
    let mut message = args.trim();
    let mut model = None;
    if let Some(rest) = message.strip_prefix("--model=") {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        model = Some(rest[..end].to_owned());
        message = rest[end..].trim_start();
    } else if message == "--model"
        || message
            .strip_prefix("--model")
            .is_some_and(|s| s.starts_with(char::is_whitespace))
    {
        let rest = message[7..].trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        model = Some(rest[..end].to_owned());
        message = rest[end..].trim_start();
    }
    if message.is_empty()
        || model
            .as_ref()
            .is_some_and(|m| m.is_empty() || m.starts_with('-'))
    {
        return Err(USAGE.into());
    }
    Ok((model, message.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_literal_message_and_model() {
        assert_eq!(
            parse("Isn't Codex's answer wrong?\n  Check `x`.").unwrap(),
            (None, "Isn't Codex's answer wrong?\n  Check `x`.".into())
        );
        assert_eq!(
            parse("--model claude-sonnet-5-5 a  b\n--flag 'x'").unwrap(),
            (Some("claude-sonnet-5-5".into()), "a  b\n--flag 'x'".into())
        );
        assert_eq!(
            parse("--model=claude-haiku-5-5 hello")
                .unwrap()
                .0
                .as_deref(),
            Some("claude-haiku-5-5")
        );
        for args in ["", "--model", "--model x", "--model= hello"] {
            assert!(parse(args).is_err());
        }
    }
}
