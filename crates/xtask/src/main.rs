//! Workspace automation. Run with `cargo xtask <command>`.
//!
//! `audit-deps` implements the offline guarantee from SPEC.md section 4.4: no
//! networking crate may appear anywhere in the dependency tree. The project is
//! offline by design (section 1.3 rules out cloud sync, telemetry, accounts,
//! licensing and auto-update), and the cheapest way to keep that true is to
//! make it a build failure rather than a policy people remember.
//!
//! The one permitted exception is the localhost/USB link to the companion app,
//! which SPEC.md section 4.4 confines to `rc-mobile` behind a feature flag. It
//! needs no crate at all - it is `std::net` - so a crate audit cannot see it.
//! The second half of the audit therefore reads the source: socket types may
//! appear only in `rc-mobile/src/bridge.rs` (and its test), that file must be
//! behind the `bridge` feature, and every `bind` in it must name loopback.

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
/// Empty: the bridge uses `std::net`, checked by `audit_sources`.
const ALLOWED_NETWORK_OWNERS: &[&str] = &[];

/// Source text that means a socket.
const SOCKET_WORDS: &[&str] = &[
    "std::net",
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "ToSocketAddrs",
    "WSAStartup",
    "Win32_Networking",
];

/// The only files allowed to contain them, relative to the workspace root.
const SOCKET_FILES: &[&str] = &[
    "crates/rc-mobile/src/bridge.rs",
    "crates/rc-mobile/tests/bridge.rs",
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    let ok = match cmd {
        "audit-deps" => audit_deps(),
        "audit-gui" => audit_gui(),
        "audit-android" => audit_android(),
        "audit" => audit_deps() && audit_gui() && audit_android(),
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
         audit-gui    The same for the desktop GUI's separate workspace\n  \
         audit-android  The companion app: permissions, dependencies, sockets\n  \
         audit        All three audits\n  \
         ci         fmt check, clippy, audit-deps, and the full test suite\n"
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

    let sources_ok = audit_sources();
    if violations.is_empty() {
        println!("  PASS: no networking crate in the dependency tree.");
        sources_ok
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

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Socket use in source, outside the one file allowed it.
fn audit_sources() -> bool {
    let root = workspace_root();
    let mut files = Vec::new();
    let mut stack = vec![root.join("crates")];
    for extra in ["gui", "companion-android"] {
        stack.push(root.join(extra));
    }
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !matches!(
                    name.as_str(),
                    "target" | "node_modules" | "build" | ".gradle"
                ) {
                    stack.push(p);
                }
            } else if name.ends_with(".rs") {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut bad = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(&root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "crates/xtask/src/main.rs" {
            continue; // this file names the words in order to forbid them
        }
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        let allowed = SOCKET_FILES.contains(&rel.as_str());
        for (n, line) in text.lines().enumerate() {
            if SOCKET_WORDS.iter().any(|w| line.contains(w)) && !allowed {
                bad.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
            if allowed && line.contains(".bind(") && !line.contains("LOCALHOST") {
                bad.push(format!(
                    "{rel}:{}: bind without LOCALHOST: {}",
                    n + 1,
                    line.trim()
                ));
            }
        }
    }
    // The bridge module must only exist behind its feature.
    let lib = std::fs::read_to_string(root.join("crates/rc-mobile/src/lib.rs")).unwrap_or_default();
    if !lib.contains("#[cfg(feature = \"bridge\")]\npub mod bridge;") {
        bad.push(
            "crates/rc-mobile/src/lib.rs: `mod bridge` is not behind the bridge feature".into(),
        );
    }
    println!("  source files checked for sockets: {}", files.len());
    if bad.is_empty() {
        println!("  PASS: sockets appear only in the loopback bridge, behind its feature.");
        true
    } else {
        println!("  FAIL: socket use outside crates/rc-mobile/src/bridge.rs:");
        for b in &bad {
            println!("    {b}");
        }
        false
    }
}

// ---------------------------------------------------------------------------
// audit-gui
// ---------------------------------------------------------------------------

/// The GUI is its own workspace because Tauri needs tokio. Tokio is allowed
/// there only as a task runtime: its `net` feature must be off, and every
/// other forbidden crate stays forbidden. The web view must not be able to
/// load anything from outside the app, and may use no plugins.
fn audit_gui() -> bool {
    println!("== GUI audit (no-network guarantee for the desktop app) ==");
    let root = workspace_root();
    let manifest = root.join("gui/src-tauri/Cargo.toml");
    let mut ok = true;

    // `cargo tree -e normal` is what is actually compiled and shipped:
    // `cargo metadata` resolves optional dependencies whether or not this
    // feature selection builds them, and dev- and build-dependencies never
    // ship. One line per package, as "name version features".
    let out = match Command::new(cargo())
        .args([
            "tree",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}|{f}",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            eprintln!("cargo tree failed: {}", String::from_utf8_lossy(&o.stderr));
            return false;
        }
        Err(e) => {
            eprintln!("could not run cargo tree: {e}");
            return false;
        }
    };
    let mut built: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in out.lines() {
        let Some((pkg, feats)) = line.split_once('|') else {
            continue;
        };
        let name = pkg.split_whitespace().next().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        built.insert(
            name,
            feats
                .split(',')
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect(),
        );
    }
    println!("  packages compiled into the GUI: {}", built.len());
    for forbidden in FORBIDDEN {
        let Some(feats) = built.get(*forbidden) else {
            continue;
        };
        if *forbidden == "tokio" {
            let bad: Vec<&String> = feats
                .iter()
                .filter(|f| matches!(f.as_str(), "net" | "full" | "process" | "signal"))
                .collect();
            if bad.is_empty() {
                println!("  tokio is present as Tauri's task runtime, with no networking feature");
            } else {
                println!("  FAIL: tokio is built with {bad:?}");
                ok = false;
            }
            continue;
        }
        println!("  FAIL: {forbidden} is compiled into the GUI");
        ok = false;
    }

    // The web view: a CSP that permits only the app itself and Tauri's IPC.
    let conf =
        std::fs::read_to_string(root.join("gui/src-tauri/tauri.conf.json")).unwrap_or_default();
    let conf: serde_json::Value = serde_json::from_str(&conf).unwrap_or_default();
    let csp = conf["app"]["security"]["csp"].as_str().unwrap_or("");
    let allowed_hosts = ["http://ipc.localhost"];
    let external: Vec<&str> = csp
        .split([' ', ';'])
        .filter(|t| t.contains("://") && !allowed_hosts.contains(t))
        .collect();
    if !csp.contains("default-src 'self'") || !external.is_empty() {
        println!("  FAIL: the web view's CSP allows outside content: {csp:?}");
        ok = false;
    } else {
        println!("  CSP: only the app and Tauri IPC");
    }
    let caps = std::fs::read_to_string(root.join("gui/src-tauri/capabilities/default.json"))
        .unwrap_or_default();
    let caps: serde_json::Value = serde_json::from_str(&caps).unwrap_or_default();
    let no_perms = Vec::new();
    let perms: Vec<&str> = caps["permissions"]
        .as_array()
        .unwrap_or(&no_perms)
        .iter()
        .filter_map(|p| p.as_str())
        .collect();
    // The app's own commands, and the system's file-choosing dialog (so a
    // person picks folders instead of typing paths). Nothing that reaches a
    // network, runs a program, or reads files from the web view.
    const ALLOWED_CAPS: &[&str] = &["core:default", "dialog:allow-open"];
    let extra: Vec<&&str> = perms.iter().filter(|p| !ALLOWED_CAPS.contains(p)).collect();
    if !extra.is_empty() || !perms.contains(&"core:default") {
        println!("  FAIL: capabilities grant more than {ALLOWED_CAPS:?}: {extra:?}");
        ok = false;
    } else {
        println!("  capabilities: {perms:?} (no http, no shell, no filesystem access)");
    }

    // The built frontend, if present, must not reach for the network itself.
    let dist = root.join("gui/dist/assets");
    if let Ok(rd) = std::fs::read_dir(&dist) {
        for e in rd.filter_map(|e| e.ok()) {
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            for word in [
                "WebSocket(",
                "XMLHttpRequest",
                "navigator.sendBeacon",
                "EventSource(",
            ] {
                if text.contains(word) {
                    println!("  FAIL: {} uses {word}", e.path().display());
                    ok = false;
                }
            }
        }
    } else {
        println!("  note: gui/dist not built; frontend bundle not checked");
    }
    println!("  {}", if ok { "PASS" } else { "FAIL" });
    ok
}

// ---------------------------------------------------------------------------
// audit-android
// ---------------------------------------------------------------------------

/// Permissions the companion app is allowed to ask for (SPEC.md 6.4: "No
/// ads, no analytics, no network permission except the local bridge").
const ANDROID_PERMISSIONS: &[&str] = &[
    "android.permission.READ_MEDIA_IMAGES",
    "android.permission.READ_MEDIA_VIDEO",
    "android.permission.READ_EXTERNAL_STORAGE",
    // Android requires this even for a socket to 127.0.0.1, which is all the
    // app opens; Bridge.kt refuses any address that is not loopback.
    "android.permission.INTERNET",
];

/// Libraries that would give the app a way to talk to a network.
const ANDROID_FORBIDDEN_DEPS: &[&str] = &[
    "okhttp",
    "retrofit",
    "ktor",
    "volley",
    "firebase",
    "crashlytics",
    "play-services",
    "analytics",
    "appcenter",
    "sentry",
    "amplitude",
    "mixpanel",
    "grpc",
    "socket.io",
];

/// Java/Kotlin API names that reach the network or run in the background.
const ANDROID_FORBIDDEN_API: &[&str] = &[
    "HttpURLConnection",
    "URLConnection",
    "WebSocket",
    "DatagramSocket",
    "ServerSocket",
    "WebView",
    "JobScheduler",
    "WorkManager",
    "startForegroundService",
    "BOOT_COMPLETED",
];

/// The one file allowed to open a socket, and what it must contain.
const ANDROID_SOCKET_FILE: &str = "Bridge.kt";

fn audit_android() -> bool {
    println!("== companion app audit (no network beyond the USB bridge) ==");
    let root = workspace_root().join("companion-android");
    if !root.is_dir() {
        println!("  companion-android is not present");
        return false;
    }
    let mut ok = true;

    let manifest =
        std::fs::read_to_string(root.join("app/src/main/AndroidManifest.xml")).unwrap_or_default();
    let mut asked = Vec::new();
    for (i, _) in manifest.match_indices("uses-permission") {
        let rest = &manifest[i..];
        if let Some(start) = rest.find("android:name=\"") {
            let from = i + start + 14;
            if let Some(end) = manifest[from..].find('"') {
                asked.push(manifest[from..from + end].to_string());
            }
        }
    }
    asked.sort();
    asked.dedup();
    for p in &asked {
        if !ANDROID_PERMISSIONS.contains(&p.as_str()) {
            println!("  FAIL: the app asks for {p}");
            ok = false;
        }
    }
    println!("  permissions asked: {}", asked.join(", "));
    for tag in ["<service", "<receiver", "<provider"] {
        if manifest.contains(tag) {
            println!("  FAIL: the manifest declares a {tag}> - the app must have no background component");
            ok = false;
        }
    }

    let gradle = std::fs::read_to_string(root.join("app/build.gradle.kts")).unwrap_or_default();
    for dep in ANDROID_FORBIDDEN_DEPS {
        if gradle.to_lowercase().contains(dep) {
            println!("  FAIL: the app depends on {dep}");
            ok = false;
        }
    }

    // Kotlin sources: sockets only in the bridge, and only to loopback.
    let mut files = Vec::new();
    let mut stack = vec![root.join("app/src")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "kt") {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut bridge_seen = false;
    for f in &files {
        let name = f
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let text = std::fs::read_to_string(f).unwrap_or_default();
        let is_bridge = name == ANDROID_SOCKET_FILE;
        bridge_seen |= is_bridge;
        for api in ANDROID_FORBIDDEN_API {
            if text.contains(api) {
                println!("  FAIL: {name} uses {api}");
                ok = false;
            }
        }
        if !is_bridge && (text.contains("Socket(") || text.contains("java.net")) {
            println!("  FAIL: {name} opens a socket; only {ANDROID_SOCKET_FILE} may");
            ok = false;
        }
        if is_bridge && !text.contains("isLoopbackAddress") {
            println!("  FAIL: {name} does not check that the address is loopback");
            ok = false;
        }
    }
    if !bridge_seen {
        println!("  FAIL: {ANDROID_SOCKET_FILE} is missing");
        ok = false;
    }
    println!("  Kotlin files checked: {}", files.len());
    println!("  {}", if ok { "PASS" } else { "FAIL" });
    ok
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
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    );
    ok &= audit_deps();
    ok &= audit_gui();
    ok &= audit_android();
    ok &= step("cargo test", &["test", "--workspace", "--all-features"]);

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
