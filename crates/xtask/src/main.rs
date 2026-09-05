//! Workspace automation. Run with `cargo xtask <command>`.
//!
//! `audit-deps` implements the offline guarantee from SPEC.md section 4.4: no
//! networking crate may appear anywhere in the dependency tree. The project is
//! offline by design (section 1.3 rules out cloud sync, telemetry, accounts,
//! licensing and auto-update), and the cheapest way to keep that true is to
//! make it a build failure rather than a policy people remember.
//!
//! The one permitted exception is the localhost/USB link to the companion app,
//! which SPEC.md section 4.4 confines to `rc-mobile` behind a feature flag.
//! That crate does not exist yet; when it does, add it to `ALLOWED_NETWORK_OWNERS`
//! and the audit will still refuse the same crates everywhere else.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, ExitCode};

/// Crates that speak to a network, or exist to make that easy.
const FORBIDDEN: &[&str] = &[
    // HTTP clients and servers
    "reqwest",
    "hyper",
    "hyper-util",
    "h2",
    "h3",
    "isahc",
    "surf",
    "ureq",
    "attohttpc",
    "curl",
    "curl-sys",
    "actix-web",
    "axum",
    "warp",
    "tide",
    "rouille",
    "tiny_http",
    // async runtimes with networking, and raw socket layers
    "tokio",
    "async-std",
    "smol",
    "mio",
    "socket2",
    "polling",
    "async-io",
    // websockets / RPC
    "tungstenite",
    "tokio-tungstenite",
    "ws",
    "tonic",
    "grpcio",
    // DNS and URL fetching
    "trust-dns-resolver",
    "hickory-resolver",
    "dns-lookup",
    // TLS stacks: only meaningful if something is talking to a network
    "native-tls",
    "openssl",
    "openssl-sys",
    "rustls",
    "tokio-rustls",
    // telemetry / crash reporting, explicitly out of scope
    "sentry",
    "opentelemetry",
    "prometheus",
];

/// Crates permitted to pull in a networking dependency, per SPEC.md 4.4.
/// Empty until `rc-mobile` lands.
const ALLOWED_NETWORK_OWNERS: &[&str] = &[];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    let ok = match cmd {
        "audit-deps" => audit_deps(),
        "ci" => ci(),
        "help" | "--help" | "-h" => {
            usage();
            true
        }
        other => {
            eprintln!("unknown command: {other}\n");
            usage();
            false
        }
    };

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn usage() {
    eprintln!(
        "cargo xtask <command>\n\n\
         Commands:\n  \
         audit-deps   Fail if any networking crate is in the dependency tree\n  \
         ci           fmt check, clippy, audit-deps, and the full test suite\n"
    );
}

// ---------------------------------------------------------------------------
// audit-deps
// ---------------------------------------------------------------------------

fn audit_deps() -> bool {
    println!("== dependency audit (no-network guarantee) ==");

    let out = match Command::new(cargo())
        .args(["metadata", "--format-version", "1", "--all-features"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            eprintln!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return false;
        }
        Err(e) => {
            eprintln!("could not run cargo metadata: {e}");
            return false;
        }
    };

    let meta: serde_json::Value = match serde_json::from_slice(&out) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("could not parse cargo metadata: {e}");
            return false;
        }
    };

    let empty = Vec::new();
    let packages = meta["packages"].as_array().unwrap_or(&empty);

    // name -> set of packages that depend on it, so a violation is actionable.
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut present: BTreeSet<String> = BTreeSet::new();

    for pkg in packages {
        let pname = pkg["name"].as_str().unwrap_or("?").to_string();
        present.insert(pname.clone());
        if let Some(deps) = pkg["dependencies"].as_array() {
            for d in deps {
                // Dev-dependencies never ship in the product.
                if d["kind"].as_str() == Some("dev") {
                    continue;
                }
                if let Some(dname) = d["name"].as_str() {
                    dependents
                        .entry(dname.to_string())
                        .or_default()
                        .insert(pname.clone());
                }
            }
        }
    }

    let mut violations: Vec<(String, Vec<String>)> = Vec::new();
    for forbidden in FORBIDDEN {
        if !present.contains(*forbidden) {
            continue;
        }
        let owners: Vec<String> = dependents
            .get(*forbidden)
            .map(|s| {
                s.iter()
                    .filter(|o| !ALLOWED_NETWORK_OWNERS.contains(&o.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !owners.is_empty() {
            violations.push(((*forbidden).to_string(), owners));
        }
    }

    println!("  packages in tree: {}", present.len());
    println!("  forbidden names checked: {}", FORBIDDEN.len());

    if violations.is_empty() {
        println!("  PASS: no networking crate in the dependency tree.");
        true
    } else {
        println!("  FAIL: networking crates found:");
        for (name, owners) in &violations {
            println!("    {name}  (pulled in by: {})", owners.join(", "));
        }
        println!(
            "\nSPEC.md section 1.3 and 4.4 make this project offline-only. If a \
             dependency is genuinely required for the companion-app bridge, confine \
             it to rc-mobile behind a feature flag and add that crate to \
             ALLOWED_NETWORK_OWNERS in crates/xtask/src/main.rs."
        );
        false
    }
}

// ---------------------------------------------------------------------------
// ci
// ---------------------------------------------------------------------------

fn ci() -> bool {
    let mut ok = true;

    ok &= step("cargo fmt --check", &["fmt", "--all", "--", "--check"]);
    ok &= step(
        "cargo clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    );
    ok &= audit_deps();
    ok &= step("cargo test", &["test", "--workspace"]);

    println!();
    if ok {
        println!("== CI PASSED ==");
    } else {
        println!("== CI FAILED ==");
    }
    ok
}

fn step(label: &str, args: &[&str]) -> bool {
    println!("\n== {label} ==");
    match Command::new(cargo()).args(args).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("  FAILED ({s})");
            false
        }
        Err(e) => {
            eprintln!("  could not run: {e}");
            false
        }
    }
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}
