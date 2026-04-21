//! `braid ups status` -- minimal operator inspection of NUT state.
//!
//! This is the same view that preflight consults. Missing config prints a
//! helpful enable-hint and exits 0 -- `braid ups status` on a pool without
//! UPS is not an error. Daemon-down (non-zero `upsc` exit) is a hard error
//! with a pointer at the upsd unit. Anything richer (stable `--json`
//! shape, curated summary with watts / runtime / battery mfr date) is
//! plan 2's responsibility.

use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{config_read, ConfigError, Ups};
use crate::parse::parse_upsc;
use crate::parse::types::{UpscOutput, UpsStatusFlag};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum UpsError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("ups daemon not running -- check 'systemctl status upsd.service'")]
    DaemonDown,
}

pub fn cmd_ups_status<R: CommandRunner>(
    runner: &R,
    config_path: &Path,
) -> Result<(), UpsError> {
    let config = config_read(config_path)?;
    let Some(ups_cfg) = config.ups() else {
        println!(
            "UPS support is not enabled. Set `braid.ups.enable = true` in\n\
             your NixOS configuration and rebuild to enable preflight\n\
             safety and low-battery shutdown."
        );
        return Ok(());
    };
    if !ups_cfg.enable {
        println!(
            "UPS support is configured but disabled. Set `braid.ups.enable = true`\n\
             in your NixOS configuration and rebuild."
        );
        return Ok(());
    }
    print_status(runner, ups_cfg)
}

fn print_status<R: CommandRunner>(runner: &R, ups_cfg: &Ups) -> Result<(), UpsError> {
    let raw = match runner.run(&CmdRequest::UpscQuery {
        name: ups_cfg.name.clone(),
    }) {
        Ok(r) => r,
        Err(_) => return Err(UpsError::DaemonDown),
    };
    let parsed = match parse_upsc(&raw) {
        Ok(p) => p,
        Err(_) => return Err(UpsError::DaemonDown),
    };
    render(&ups_cfg.name, &parsed);
    Ok(())
}

fn render(name: &str, parsed: &UpscOutput) {
    println!("UPS: {}", name);
    println!("Status: {}", format_status(&parsed.status_flags));
    if !parsed.extra.is_empty() {
        println!();
        for (k, v) in &parsed.extra {
            println!("  {}: {}", k, v);
        }
    }
}

fn format_status(flags: &std::collections::HashSet<UpsStatusFlag>) -> String {
    if flags.is_empty() {
        return "(unknown -- ups.status missing)".to_owned();
    }
    let mut tokens: Vec<String> = flags.iter().map(flag_token).collect();
    tokens.sort();
    tokens.join(" ")
}

fn flag_token(flag: &UpsStatusFlag) -> String {
    match flag {
        UpsStatusFlag::Ol => "OL".into(),
        UpsStatusFlag::Ob => "OB".into(),
        UpsStatusFlag::Lb => "LB".into(),
        UpsStatusFlag::Rb => "RB".into(),
        UpsStatusFlag::Hb => "HB".into(),
        UpsStatusFlag::Chrg => "CHRG".into(),
        UpsStatusFlag::Dischrg => "DISCHRG".into(),
        UpsStatusFlag::Cal => "CAL".into(),
        UpsStatusFlag::Bypass => "BYPASS".into(),
        UpsStatusFlag::Off => "OFF".into(),
        UpsStatusFlag::Over => "OVER".into(),
        UpsStatusFlag::Trim => "TRIM".into(),
        UpsStatusFlag::Boost => "BOOST".into(),
        UpsStatusFlag::Fsd => "FSD".into(),
        UpsStatusFlag::Unknown(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Intent: format_status renders OL verbatim when the UPS is on utility power.
    // Why: operators triaging preflight failures need the rendered line to
    // match NUT's own token vocabulary; translating OL to "online" would
    // force them to cross-reference wording.
    // Scenario: `braid ups status` against a healthy UPS.
    #[test]
    fn format_status_ol() {
        let mut flags = std::collections::HashSet::new();
        flags.insert(UpsStatusFlag::Ol);
        assert_eq!(format_status(&flags), "OL");
    }

    // Intent: multi-flag status renders every token, sorted for stability.
    // Why: sorting makes unit tests deterministic and lets operators diff
    // two renders without spurious reordering noise.
    // Scenario: critical state -- UPS on battery, low battery threshold hit.
    #[test]
    fn format_status_ob_lb_sorted() {
        let mut flags = std::collections::HashSet::new();
        flags.insert(UpsStatusFlag::Ob);
        flags.insert(UpsStatusFlag::Lb);
        assert_eq!(format_status(&flags), "LB OB");
    }

    // Intent: empty flag set renders an explicit `unknown` sentinel.
    // Why: preflight fails closed on an empty set; `braid ups status` needs
    // to print something the operator can act on, not a blank line.
    // Scenario: dummy-ups fixture with no ups.status line yet.
    #[test]
    fn format_status_empty_is_unknown() {
        let flags = std::collections::HashSet::new();
        let rendered = format_status(&flags);
        assert!(rendered.contains("unknown"));
    }
}
