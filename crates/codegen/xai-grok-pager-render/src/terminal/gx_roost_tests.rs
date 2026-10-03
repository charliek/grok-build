//! gx: Roost (`TERM_PROGRAM=Roost`) negotiates KKP like a recognised terminal.

use super::{MultiplexerKind, TerminalName, build_terminal_context_from_env, env_from};

#[test]
fn roost_is_probed_for_kkp_and_advertises_shift_enter() {
    let ctx = build_terminal_context_from_env(&env_from(&[
        ("TERM_PROGRAM", "Roost"),
        ("TERM", "xterm-256color"),
    ]));
    assert_eq!(ctx.brand, TerminalName::Unknown);
    assert!(ctx.gx_roost_host);
    assert_eq!(ctx.kitty_skip_reason(), None);
    assert!(!ctx.shift_enter_unavailable());
}

#[test]
fn roost_match_ignores_case_and_padding() {
    for value in ["roost", "ROOST", " Roost "] {
        let ctx = build_terminal_context_from_env(&env_from(&[("TERM_PROGRAM", value)]));
        assert!(ctx.gx_roost_host, "{value:?}");
    }
}

#[test]
fn other_unknown_terminals_still_skip_kkp() {
    for env in [
        env_from(&[("TERM", "xterm-256color")]),
        env_from(&[("TERM_PROGRAM", "Roostish"), ("TERM", "xterm-256color")]),
    ] {
        let ctx = build_terminal_context_from_env(&env);
        assert!(!ctx.gx_roost_host);
        assert_eq!(ctx.kitty_skip_reason(), Some("unknown_no_multiplexer"));
        assert!(ctx.shift_enter_unavailable());
    }
}

#[test]
fn a_multiplexer_inside_roost_keeps_its_own_verdict() {
    let ctx = build_terminal_context_from_env(&env_from(&[
        ("TERM_PROGRAM", "Roost"),
        ("STY", "1234.pts-0.host"),
    ]));
    assert_eq!(ctx.multiplexer, MultiplexerKind::Screen);
    assert_eq!(ctx.kitty_skip_reason(), Some("screen"));
}

fn roost() -> super::TerminalContext {
    build_terminal_context_from_env(&env_from(&[("TERM_PROGRAM", "Roost")]))
}

#[test]
fn old_tmux_inside_roost_still_skips_kkp() {
    for version in [Some("tmux 3.2".to_owned()), None] {
        let ctx = super::TerminalContext {
            multiplexer: MultiplexerKind::Tmux,
            tmux_version: version,
            ..roost()
        };
        assert_eq!(ctx.kitty_skip_reason(), Some("tmux_old"));
        assert!(ctx.prefer_alt_enter_newline());
    }
}

#[test]
fn tmux_extended_keys_off_inside_roost_still_skips_kkp() {
    let ctx = super::TerminalContext {
        multiplexer: MultiplexerKind::Tmux,
        tmux_version: Some("tmux 3.4".to_owned()),
        tmux_extended_keys: Some("off".to_owned()),
        ..roost()
    };
    assert_eq!(ctx.kitty_skip_reason(), Some("tmux_extended_keys_off"));
    assert!(ctx.prefer_alt_enter_newline());
}

#[test]
fn modern_tmux_with_extended_keys_inside_roost_negotiates() {
    let ctx = super::TerminalContext {
        multiplexer: MultiplexerKind::Tmux,
        tmux_version: Some("tmux 3.4".to_owned()),
        tmux_extended_keys: Some("on".to_owned()),
        ..roost()
    };
    assert_eq!(ctx.kitty_skip_reason(), None);
}

#[test]
fn ssh_from_roost_still_prefers_alt_enter() {
    let ctx = super::TerminalContext {
        is_ssh: true,
        ..roost()
    };
    assert!(!ctx.shift_enter_unavailable());
    assert!(ctx.prefer_alt_enter_newline());
}
