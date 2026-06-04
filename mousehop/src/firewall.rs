//! `mousehop firewall` — open the mousehop UDP port in the host
//! firewall.
//!
//! mousehop's transport is DTLS over UDP on a single port. Many Linux
//! distros ship a default-deny firewall (Ubuntu's `ufw`, Fedora's
//! `firewalld`, or plain `nftables`), and Windows Firewall blocks
//! inbound UDP by default — so an *inbound* connection from a peer is
//! dropped even though outbound works. This subcommand detects the
//! active firewall and adds (or removes) the matching allow rule.
//!
//! Adding rules requires elevation, so this is meant to be run as
//! `sudo mousehop firewall` (Linux) / from an elevated shell
//! (Windows). For firewalls whose rule layout varies (nftables,
//! iptables) we *print* the exact command instead of guessing where to
//! splice it.

use std::process::Command;

/// One firewall backend's plan: the human label, whether we run the
/// commands ourselves or just print them for the user, and the command
/// lines (each a program + args).
struct Plan {
    backend: &'static str,
    /// Run the commands; if false, only print them (rule layout is
    /// site-specific and unsafe to auto-edit).
    apply: bool,
    commands: Vec<Vec<String>>,
    /// Extra guidance printed after the commands.
    note: Option<String>,
}

/// Entry point for the `firewall` subcommand. Returns a process exit
/// code.
pub fn run(port: u16, remove: bool, dry_run: bool) -> i32 {
    let plan = match plan_for_host(port, remove) {
        Some(p) => p,
        None => {
            eprintln!(
                "mousehop firewall: no supported firewall detected. Allow UDP port {port} \
                 inbound manually if your host blocks it."
            );
            return 0;
        }
    };

    let verb = if remove { "Removing" } else { "Adding" };
    println!("{verb} a rule for UDP/{port} via {} ...", plan.backend);

    if dry_run || !plan.apply {
        if !plan.apply && !dry_run {
            println!(
                "Your {} ruleset is site-specific, so run this yourself:",
                plan.backend
            );
        } else {
            println!("Would run:");
        }
        for cmd in &plan.commands {
            println!("  {}", shell_join(cmd));
        }
        if let Some(note) = &plan.note {
            println!("{note}");
        }
        return 0;
    }

    for cmd in &plan.commands {
        println!("+ {}", shell_join(cmd));
        match Command::new(&cmd[0]).args(&cmd[1..]).status() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!(
                    "mousehop firewall: `{}` failed ({status}). \
                     You probably need to re-run with sudo / as administrator.",
                    shell_join(cmd)
                );
                return 1;
            }
            Err(e) => {
                eprintln!("mousehop firewall: could not run `{}`: {e}", cmd[0]);
                return 1;
            }
        }
    }
    if let Some(note) = &plan.note {
        println!("{note}");
    }
    println!("Done.");
    0
}

fn shell_join(cmd: &[String]) -> String {
    cmd.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn s(v: &str) -> String {
    v.to_string()
}

#[cfg(target_os = "linux")]
fn plan_for_host(port: u16, remove: bool) -> Option<Plan> {
    let pu = format!("{port}/udp");
    // Prefer the higher-level managers (ufw, firewalld) — they own the
    // ruleset and have stable, idempotent CLIs — before touching
    // nft/iptables directly.
    if service_active("ufw") {
        let cmd = if remove {
            vec![s("ufw"), s("delete"), s("allow"), pu]
        } else {
            vec![s("ufw"), s("allow"), pu, s("comment"), s("mousehop")]
        };
        return Some(Plan {
            backend: "ufw",
            apply: true,
            commands: vec![cmd],
            note: None,
        });
    }
    if service_active("firewalld") {
        let flag = if remove {
            "--remove-port"
        } else {
            "--add-port"
        };
        return Some(Plan {
            backend: "firewalld",
            apply: true,
            commands: vec![
                vec![
                    s("firewall-cmd"),
                    s("--permanent"),
                    format!("{flag}={port}/udp"),
                ],
                vec![s("firewall-cmd"), s("--reload")],
            ],
            note: None,
        });
    }
    if service_active("nftables") || command_exists("nft") {
        // Chain/table names are site-specific; print rather than guess.
        let verb = if remove { "delete" } else { "add" };
        return Some(Plan {
            backend: "nftables",
            apply: false,
            commands: vec![vec![
                s("nft"),
                s(verb),
                s("rule"),
                s("inet"),
                s("filter"),
                s("input"),
                s("udp"),
                s("dport"),
                port.to_string(),
                s("accept"),
            ]],
            note: Some(s(
                "  (substitute your actual table/chain — list them with `nft list ruleset`)",
            )),
        });
    }
    if command_exists("iptables") {
        let verb = if remove { "-D" } else { "-A" };
        return Some(Plan {
            backend: "iptables",
            apply: false,
            commands: vec![
                vec![
                    s("iptables"),
                    s(verb),
                    s("INPUT"),
                    s("-p"),
                    s("udp"),
                    s("--dport"),
                    port.to_string(),
                    s("-j"),
                    s("ACCEPT"),
                ],
                vec![
                    s("ip6tables"),
                    s(verb),
                    s("INPUT"),
                    s("-p"),
                    s("udp"),
                    s("--dport"),
                    port.to_string(),
                    s("-j"),
                    s("ACCEPT"),
                ],
            ],
            note: Some(s(
                "  (and persist them with your distro's iptables-save mechanism)",
            )),
        });
    }
    None
}

#[cfg(target_os = "windows")]
fn plan_for_host(port: u16, remove: bool) -> Option<Plan> {
    let name = format!("Mousehop (UDP {port})");
    let commands = if remove {
        vec![vec![
            s("netsh"),
            s("advfirewall"),
            s("firewall"),
            s("delete"),
            s("rule"),
            format!("name={name}"),
        ]]
    } else {
        vec![vec![
            s("netsh"),
            s("advfirewall"),
            s("firewall"),
            s("add"),
            s("rule"),
            format!("name={name}"),
            s("dir=in"),
            s("action=allow"),
            s("protocol=UDP"),
            format!("localport={port}"),
        ]]
    };
    Some(Plan {
        backend: "Windows Firewall",
        apply: true,
        commands,
        note: Some(s("  (run from an Administrator command prompt)")),
    })
}

#[cfg(target_os = "macos")]
fn plan_for_host(port: u16, _remove: bool) -> Option<Plan> {
    Some(Plan {
        backend: "macOS",
        apply: false,
        commands: vec![],
        note: Some(format!(
            "  macOS's application firewall is per-app and off by default; a signed\n  \
             Mousehop.app needs no port rule. If you've enabled it, allow the app under\n  \
             System Settings > Network > Firewall. (UDP port {port}.)"
        )),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn plan_for_host(_port: u16, _remove: bool) -> Option<Plan> {
    None
}

#[cfg(target_os = "linux")]
fn service_active(name: &str) -> bool {
    // `systemctl is-active` needs no root and prints "active" on stdout.
    Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or_else(|_| {
            // Some tools (iptables) print version to stderr / exit 0 only
            // as root; fall back to a PATH probe.
            which(name)
        })
}

#[cfg(target_os = "linux")]
fn which(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
