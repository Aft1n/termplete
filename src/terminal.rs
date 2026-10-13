//! Terminal application detection and per-application exclusions.

pub const APPS: &[(&str, &str)] = &[
    ("warp", "Warp"),
    ("iterm", "iTerm2"),
    ("apple_terminal", "macOS Terminal"),
    ("vscode", "VS Code"),
    ("wezterm", "WezTerm"),
    ("ghostty", "Ghostty"),
    ("kitty", "Kitty"),
    ("alacritty", "Alacritty"),
];

pub fn normalize(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "warpterminal" => "warp".into(),
        "iterm.app" | "iterm2" => "iterm".into(),
        "terminal" | "terminal.app" | "macos terminal" => "apple_terminal".into(),
        other => other.to_string(),
    }
}

pub fn current() -> Option<String> {
    identify(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("WARP_IS_LOCAL_SHELL_SESSION").ok().as_deref(),
    )
    .or_else(|| std::env::var_os("KITTY_PID").map(|_| "kitty".into()))
    .or_else(|| std::env::var_os("ALACRITTY_SOCKET").map(|_| "alacritty".into()))
}

fn identify(program: Option<&str>, warp_session: Option<&str>) -> Option<String> {
    if warp_session == Some("1") {
        return Some("warp".into());
    }
    program.map(normalize).filter(|name| !name.is_empty())
}

pub fn disabled(names: &[String], terminal: &str) -> bool {
    let terminal = normalize(terminal);
    names.iter().any(|name| normalize(name) == terminal)
}

pub fn label(name: &str) -> String {
    let normalized = normalize(name);
    APPS.iter()
        .find(|(id, _)| *id == normalized)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| name.to_string())
}

pub fn parse_list(value: &str) -> Vec<String> {
    let mut names = Vec::new();
    for name in value
        .split(',')
        .map(normalize)
        .filter(|name| !name.is_empty())
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_warp_without_confusing_other_terminals() {
        assert_eq!(
            identify(Some("WarpTerminal"), None).as_deref(),
            Some("warp")
        );
        assert_eq!(identify(None, Some("1")).as_deref(), Some("warp"));
        assert_eq!(
            identify(Some("iTerm.app"), Some("0")).as_deref(),
            Some("iterm")
        );
        assert_eq!(identify(Some("WezTerm"), None).as_deref(), Some("wezterm"));
        assert_eq!(identify(None, None), None);
        assert_eq!(identify(Some("  "), None), None);
    }

    #[test]
    fn exclusions_match_aliases_and_custom_apps_but_not_substrings() {
        let names = parse_list("WarpTerminal, warp, iTerm.app, MY-TERMINAL");
        assert_eq!(names, ["warp", "iterm", "my-terminal"]);
        assert!(disabled(&names, "WARP"));
        assert!(disabled(&names, "iTerm2"));
        assert!(disabled(&names, "my-terminal"));
        assert!(!disabled(&names, "warpish"));
        assert!(!disabled(&names, "wezterm"));
        assert!(parse_list("").is_empty());
    }
}
