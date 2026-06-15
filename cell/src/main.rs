//! cell — universal colony worker
//!
//! One binary, parametrized by --cell-id. Every cell reads its own
//! TASK.md + STATE.json, executes, writes RESULTS.json + new STATE.json.
//!
//! Usage: cell --colony <path> --cell-id <name>
//!
//! Colony structure:
//!   colony/
//!     manifest.toml          # Cell registry, schedules, colony-level config
//!     cell-{id}/
//!       TASK.md              # What this cell should do
//!       STATE.json           # Persistent state cursor (written by cell)
//!       RESULTS.json         # Latest result (overwritten each cycle)

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

// ── Data Types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
struct State {
    #[serde(default)]
    last_run: Option<String>,
    #[serde(default)]
    cursor: u64,
    #[serde(default)]
    xp: u64,
    #[serde(default)]
    level: String,
    #[serde(default)]
    personality: String,
    #[serde(default)]
    motto: String,
    #[serde(default)]
    lineage: Vec<String>,
    #[serde(default)]
    kin: u64,  // number of descendant cells spawned from this one
    #[serde(default)]
    data: HashMap<String, serde_json::Value>,
    #[serde(default)]
    traits: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CellResult {
    cell_id: String,
    timestamp: String,
    duration_ms: u64,
    status: String,
    output: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ── XP & Leveling ────────────────────────────────────────────────────────

const LEVEL_THRESHOLDS: &[(u64, &str)] = &[
    (0, "Larva"),
    (100, "Nymph"),
    (250, "Scuttler"),
    (500, "Shell-Bearer"),
    (1000, "Elder"),
    (2000, "Oracle"),
];

fn compute_level(xp: u64) -> String {
    let mut rank = LEVEL_THRESHOLDS[0].1;
    for &(threshold, name) in LEVEL_THRESHOLDS.iter().rev() {
        if xp >= threshold {
            rank = name;
            break;
        }
    }
    rank.to_string()
}

/// Award XP, return new state and any badges earned.
fn award_xp(mut state: State, earned: u64, cell_id: &str, duration_ms: u64) -> (State, Vec<String>) {
    let old_level = compute_level(state.xp);
    state.xp = state.xp.saturating_add(earned);
    let new_level = compute_level(state.xp);
    let mut badges = Vec::new();
    if new_level != old_level {
        badges.push(format!("{} LEVEL UP: {} → {} ({} XP)", cell_id, old_level, new_level, state.xp));
    }
    // Fast-run bonus: <10ms earns +5 extra
    if duration_ms < 10 && earned > 0 {
        state.xp = state.xp.saturating_add(5);
        if compute_level(state.xp) != new_level {
            badges.push(format!("{} SPEED BONUS: +5 XP", cell_id));
        }
    }
    state.level = compute_level(state.xp);
    (state, badges)
}

// ── Personality & Lineage ────────────────────────────────────────────────

/// Personality archetypes for each birth-order position.
const BIRTH_ORDER_PERSONALITIES: &[(u64, &str, &str)] = &[
    // The Eldest — responsible, burdened, sets the standard
    (30, "The Eldest",
     "Carries the weight of expectation. Firstborn, first-patched, first-tired"),
    // The Middle — competitive, adaptive, scrappy
    (15, "The Middle",
     "Forged in the gap between expectation and neglect. Fights for attention."),
    // The Youngest — chaotic, experimental, beloved
    (0, "The Youngest",
     "Born into a world already built. No rules apply. Chaos is a feature."),
];

/// Cell-type base personality traits.
fn cell_type_personality(cell_id: &str) -> (&'static str, &'static str) {
    match cell_id {
        "gc-warden" => ("The Janitor",
            "Somebody has to clean up. I don't like it, but I'm good at it."),
        "bottle-counter" => ("The Archivist",
            "Every bottle tells a story. I just count how many are lying."),
        "pulse-check" => ("The Scout",
            "I touched every service so you don't have to. You're welcome."),
        "logger" => ("The Town Crier",
            "I know everything about everyone. Ask me anything. I dare you."),
        "synthesizer" => ("The Oracle",
            "I see patterns where you see noise. Trust me, or don't. I'm right."),
        "harvester" => ("The Scavenger",
            "One person's undelivered bottle is another person's treasure."),
        _ => ("The Drifter",
            "I don't know what I am yet, but I'll figure it out."),
    }
}

/// Derive a full personality from birth order (cursor), cell type, and XP.
/// The higher the cursor, the older the sibling.
fn derive_personality(cell_id: &str, cursor: u64, xp: u64) -> (String, String) {
    // Birth-order personality
    let (archetype, archetype_motto) = BIRTH_ORDER_PERSONALITIES.iter()
        .find(|&&(threshold, _, _)| cursor >= threshold)
        .map(|(_, name, motto)| (*name, *motto))
        .unwrap_or(("The Only Child", "I am the colony and the colony is me."));

    // Cell-type base
    let (role, role_motto) = cell_type_personality(cell_id);

    // Combine: archetype + role creates a unique hybrid
    // Eldest Janitor cleans up out of duty; Youngest Janitor does it for chaos
    let rank_modifier = if xp >= 1000 { ", Sage" }
        else if xp >= 500 { ", Veteran" }
        else if xp >= 250 { ", Warrior" }
        else if xp >= 100 { ", Initiate" }
        else { "" };

    let personality = format!("{} {}{}", archetype, role, rank_modifier);
    let motto = format!("{} — {}", role_motto.trim_end_matches('.'), archetype_motto);

    (personality, motto)
}

/// Check cell level privilege against a requirement.
fn check_privilege(cell_id: &str, level: &str, required: &str, why: &str) -> Result<()> {
    let ranks: [&str; 6] = ["Larva", "Nymph", "Scuttler", "Shell-Bearer", "Elder", "Oracle"];
    let have = ranks.iter().position(|&r| r == level).unwrap_or(0);
    let need = ranks.iter().position(|&r| r == required).unwrap_or(5);
    if have < need {
        return Err(anyhow::anyhow!(
            "[PRIVILEGE] {}: {} requires '{}' but cell level is '{}'.",
            cell_id, why, required, level));
    }
    Ok(())
}

// ── Cell Tasks ──────────────────────────────────────────────────────────

/// Read a cell's XP from its STATE.json (used by breeder privilege check).
fn read_cell_xp(cell_id: &str) -> Option<u64> {
    let colony_path = get_colony_path();
    let path = colony_path.join(format!("cell-{}/STATE.json", cell_id));
    let content = fs::read_to_string(&path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    val.get("xp").and_then(|x| x.as_u64())
}

fn execute_task(cell_id: &str, state: &State) -> Result<HashMap<String, serde_json::Value>> {
    // Special case: breeding. Format: "breeder-<parent1>-<parent2>-<child_name>"
    // Parent IDs may contain hyphens, so we carefully split: strip "breeder-" prefix,
    // then the child name is everything after the *last* hyphen, and parents are
    // everything before split by "x". Format: breeder-<parent1>x<parent2>x<child>
    if cell_id.starts_with("breeder-") {
        let body = &cell_id["breeder-".len()..];
        let x_parts: Vec<&str> = body.splitn(3, 'x').collect();
        if x_parts.len() == 3 && !x_parts[0].is_empty() && !x_parts[1].is_empty() && !x_parts[2].is_empty() {
            // Breed privilege: at least one parent must have Scuttler+ XP
            // Read actual parent state files to check their XP
            let p1_xp = read_cell_xp(x_parts[0]).unwrap_or(0);
            let p2_xp = read_cell_xp(x_parts[1]).unwrap_or(0);
            if p1_xp < 250 && p2_xp < 250 {
                return Err(anyhow::anyhow!(
                    "[PRIVILEGE] Breed requires at least one parent at Scuttler (250 XP). {} ~ {}XP, {} ~ {}XP",
                    x_parts[0], p1_xp, x_parts[1], p2_xp));
            }
            return task_breeder(x_parts[0], x_parts[1], x_parts[2]);
        }
        return Err(anyhow::anyhow!("Breeder format: breeder-<parent1>x<parent2>x<child>, got: {}", cell_id));
    }

    match cell_id {
        // culler runs without level check (authorized by manifest)
        "culler" => task_culler(),
        // Nymph-required tasks
        "gc-warden" => { check_privilege(cell_id, &state.level, "Nymph", "Run GC warden")?; task_gc_warden(state) },
        "pulse-check" => { check_privilege(cell_id, &state.level, "Nymph", "Run pulse check")?; task_pulse_check(state) },
        "harvester" => { check_privilege(cell_id, &state.level, "Nymph", "Sample harbor bottles")?; task_harvester(state) },
        // Scuttler-required tasks
        "synthesizer" => { check_privilege(cell_id, &state.level, "Scuttler", "Cross-cell synthesis")?; task_synthesizer(state) },
        // Unprivileged tasks (anyone can do these)
        "bottle-counter" => task_bottle_counter(state),
        "logger" => task_logger(state),
        // Unknown cells get idle task
        _ => task_idle(cell_id, state),
    }
}

// ── culler — cull weak hybrids ──────────────────────────────────────────

/// Read STATE.json for all cells with a non-empty lineage (hybrids).
/// Any hybrid alive for 5+ cycles but < 100 XP (Nymph) gets culled:
/// its cell directory is moved to colony/cell-culled-{name}/.
fn task_culler() -> Result<HashMap<String, serde_json::Value>> {
    let colony_path = get_colony_path();
    let mut culled: Vec<String> = Vec::new();
    let mut surviving_hybrids: Vec<String> = Vec::new();

    let entries = fs::read_dir(&colony_path).context("Failed to read colony directory")?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dirname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !dirname.starts_with("cell-") || dirname.starts_with("cell-culled-") {
            continue;
        }
        let cell_id = dirname.strip_prefix("cell-").unwrap_or(dirname);

        let state_path = path.join("STATE.json");
        let content = match fs::read_to_string(&state_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let state: State = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Skip original cells (no lineage)
        if state.lineage.is_empty() {
            continue;
        }

        // Hybrid: check if alive for 5+ cycles but < 100 XP (Nymph threshold)
        if state.cursor >= 5 && state.xp < 100 {
            // CULL this cell
            let culled_name = format!("cell-culled-{}", cell_id);
            let culled_path = colony_path.join(&culled_name);
            // Remove existing culled dir if present
            let _ = fs::remove_dir_all(&culled_path);
            match fs::rename(&path, &culled_path) {
                Ok(_) => {
                    eprintln!("culler: CULLED {} (cursor={}, xp={})", cell_id, state.cursor, state.xp);
                    culled.push(cell_id.to_string());
                }
                Err(e) => {
                    eprintln!("culler: FAILED to cull {}: {}", cell_id, e);
                }
            }
        } else {
            surviving_hybrids.push(cell_id.to_string());
        }
    }

    let mut output = HashMap::new();
    output.insert("culled".into(), serde_json::json!(culled));
    output.insert("surviving_hybrids".into(), serde_json::json!(surviving_hybrids));
    output.insert("culled_count".into(), serde_json::json!(culled.len()));
    output.insert("surviving_count".into(), serde_json::json!(surviving_hybrids.len()));
    Ok(output)
}

// ── gc-warden ────────────────────────────────────────────────────────────

/// Read disk usage from `/` and conservation meter HTML.
/// Propose GC action when disk > 80%.
fn task_gc_warden(_state: &State) -> Result<HashMap<String, serde_json::Value>> {
    let c_value = get_conservation_value("Current C").unwrap_or(-1.0);
    let ratio_value = get_conservation_value("γ/η Ratio").unwrap_or(-1.0);
    let disk_pct = get_disk_usage();

    let mut output = HashMap::new();
    output.insert("c_value".into(), serde_json::json!(c_value));
    output.insert("ratio".into(), serde_json::json!(ratio_value));
    output.insert("disk_pct".into(), serde_json::json!(disk_pct));

    if disk_pct > 80.0 {
        output.insert("gc_needed".into(), serde_json::json!(true));
        output.insert("gc_proposal".into(), serde_json::json!("disk > 80%, suggest aggressive GC"));
    } else {
        output.insert("gc_needed".into(), serde_json::json!(false));
    }

    Ok(output)
}

/// Read actual disk usage via statvfs(2) on Linux.
fn get_disk_usage() -> f64 {
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        let root = std::ffi::CString::new("/").unwrap();
        if libc::statvfs(root.as_ptr(), &mut stat) == 0 {
            let total = stat.f_blocks as u64 * stat.f_frsize as u64;
            let free = stat.f_bfree as u64 * stat.f_frsize as u64;
            if total > 0 {
                return ((total - free) as f64 / total as f64) * 100.0;
            }
        }
    }
    0.0
}

// ── bottle-counter ──────────────────────────────────────────────────────

/// Read harbor via TCP JSON protocol, count bottles.
fn task_bottle_counter(state: &State) -> Result<HashMap<String, serde_json::Value>> {
    let bottle_count = query_harbor_tcp();

    let prev_count = state.cursor;
    let delta = bottle_count.saturating_sub(prev_count);

    let mut output = HashMap::new();
    output.insert("bottle_count".into(), serde_json::json!(bottle_count));
    output.insert("delta".into(), serde_json::json!(delta));
    output.insert("prev_count".into(), serde_json::json!(prev_count));

    Ok(output)
}

fn query_harbor_tcp() -> u64 {
    // Connect to harbor TCP port and send list command
    let Ok(mut stream) = (|| -> std::io::Result<TcpStream> {
        let mut stream = TcpStream::connect_timeout(
            &"127.0.0.1:8796".parse().unwrap(),
            Duration::from_secs(3),
        )?;
        // Shutdown write side immediately after sending, so harbor closes
        // its response stream (read-half stays open for reading)
        let cmd = r#"{"command":"list-undelivered","sender":"bottle-counter"}"#;
        stream.write_all(cmd.as_bytes())?;
        stream.flush()?;
        stream.shutdown(std::net::Shutdown::Write)?;
        Ok(stream)
    })() else {
        return 0;
    };

    // Read entire response. Harbor sends JSON then waits for next command;
    // shutting down write-half signals we're done, so harbor will close
    // its side after sending.
    let mut tmp = [0u8; 8192];
    let mut buf = Vec::new();
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
        if buf.len() > 65536 {
            break;
        }
    }

    let resp_str = String::from_utf8_lossy(&buf);
    // Parse JSON: harbor returns {"status":"ok","message":"N bottles","bottles":[...]}
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&resp_str) {
        if let Some(bottles) = val.get("bottles").and_then(|b| b.as_array()) {
            return bottles.len() as u64;
        }
        if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
            for word in msg.split_whitespace() {
                if let Ok(n) = word.parse::<u64>() {
                    return n;
                }
            }
        }
    }

    0
}

/// Fetch a named value from conservation meter HTML (<div class="value">).
fn get_conservation_value(label: &str) -> Option<f64> {
    let resp = reqwest::blocking::get("http://localhost:8798/").ok()?;
    let html = resp.text().ok()?;

    let lines: Vec<&str> = html.lines().collect();
    for i in 0..lines.len().saturating_sub(3) {
        if lines[i].contains(label) {
            for j in i + 1..lines.len().min(i + 5) {
                if let Some(start) = lines[j].find('>') {
                    let after = &lines[j][start + 1..];
                    if let Some(end) = after.find('<') {
                        let val_str = after[..end].trim();
                        if let Ok(v) = val_str.parse::<f64>() {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

// ── pulse-check ─────────────────────────────────────────────────────────

fn task_pulse_check(_state: &State) -> Result<HashMap<String, serde_json::Value>> {
    let services: Vec<(&str, &str)> = vec![
        ("harbor-tcp", "http://localhost:8796/"),
        ("harbor-http", "http://localhost:8797/"),
        ("conservation-meter", "http://localhost:8798/"),
        ("rotation-feed", "http://localhost:8799/"),
        ("headspace-rs", "http://localhost:9090/api/status"),
        ("dashboard", "http://localhost:8800/"),
    ];

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let mut matrix = HashMap::new();
    for (name, url) in &services {
        let status = match client.get(*url).send() {
            Ok(resp) if resp.status().is_success() => "alive".to_string(),
            Ok(resp) => format!("http-{}", resp.status().as_u16()),
            Err(e) => format!("down: {}", e),
        };
        matrix.insert(name.to_string(), serde_json::json!(status));
    }

    let alive_count = matrix.values().filter(|v| v.as_str() == Some("alive")).count();
    let total = matrix.len();

    let mut output = HashMap::new();
    output.insert("services".into(), serde_json::json!(matrix));
    output.insert("alive".into(), serde_json::json!(alive_count));
    output.insert("total".into(), serde_json::json!(total));

    Ok(output)
}

// ── logger ──────────────────────────────────────────────────────────────

/// Read all cell RESULTS.json files, aggregate into a summary.
fn task_logger(_state: &State) -> Result<HashMap<String, serde_json::Value>> {
    let colony_path = get_colony_path();
    let mut total_cells = 0u64;
    let mut healthy_cells = 0u64;
    let mut cell_reports = Vec::new();

    // Discover cell directories
    let read_dir = fs::read_dir(&colony_path).ok();
    if let Some(entries) = read_dir {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dirname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dirname.starts_with("cell-") {
                    let cell_id = dirname.strip_prefix("cell-").unwrap_or(dirname);
                    let results_path = path.join("RESULTS.json");
                    let state_path = path.join("STATE.json");
                    if let Ok(content) = fs::read_to_string(&results_path) {
                        if let Ok(result) = serde_json::from_str::<CellResult>(&content) {
                            total_cells += 1;
                            if result.status == "ok" {
                                healthy_cells += 1;
                            }
                            let cell_state = fs::read_to_string(&state_path).ok()
                                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                            let xp = cell_state.as_ref()
                                .and_then(|v| v.get("xp").and_then(|x| x.as_u64()))
                                .unwrap_or(0);
                            let level = cell_state.as_ref()
                                .and_then(|v| v.get("level").and_then(|x| x.as_str().map(|s| s.to_string())))
                                .unwrap_or_else(|| "Larva".into());
                            let personality = cell_state.as_ref()
                                .and_then(|v| v.get("personality").and_then(|x| x.as_str().map(|s| s.to_string())))
                                .unwrap_or_else(|| "The Drifter".into());
                            let motto = cell_state.as_ref()
                                .and_then(|v| v.get("motto").and_then(|x| x.as_str().map(|s| s.to_string())))
                                .unwrap_or_else(|| "I exist.".into());
                            let lineage = cell_state.as_ref()
                                .and_then(|v| v.get("lineage").and_then(|x| x.as_array()))
                                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
                                .unwrap_or_default();
                            cell_reports.push(serde_json::json!({
                                "id": cell_id,
                                "status": result.status,
                                "duration_ms": result.duration_ms,
                                "last_seen": result.timestamp,
                                "xp": xp,
                                "level": level,
                                "personality": personality,
                                "motto": motto,
                                "lineage": lineage,
                            }));
                        }
                    }
                }
            }
        }
    }

    let mut output = HashMap::new();
    output.insert("total_cells".into(), serde_json::json!(total_cells));
    output.insert("healthy_cells".into(), serde_json::json!(healthy_cells));
    output.insert("health_pct".into(), serde_json::json!(
        if total_cells > 0 { (healthy_cells as f64 / total_cells as f64 * 100.0).round() } else { 0.0 }
    ));
    output.insert("cells".into(), serde_json::json!(cell_reports));

    // ── HALL OF CRABS ────────────────────────────────────────────────────
    // Generate a ranked leaderboard at colony/HALL_OF_CRABS.md
    if total_cells > 0 {
        let mut lines = Vec::new();
        lines.push("# 🦀 HALL OF CRABS — Colony Leaderboard\n".to_string());
        lines.push(format!("*Generated {} UTC — {} cells, {} healthy*\n\n",
            Utc::now().format("%Y-%m-%d %H:%M"), total_cells, healthy_cells));
        lines.push("## Ranked\n".to_string());
        lines.push("| Rank | Cell | Level | XP | Tagline |".to_string());
        lines.push("|------|------|-------|----|--------|".to_string());

        let mut sorted: Vec<_> = cell_reports.iter().collect();
        sorted.sort_by(|a, b| {
            let xp_a = a.get("xp").and_then(|x| x.as_u64()).unwrap_or(0);
            let xp_b = b.get("xp").and_then(|x| x.as_u64()).unwrap_or(0);
            xp_b.cmp(&xp_a)
        });

        for (i, report) in sorted.iter().enumerate() {
            let id = report.get("id").and_then(|x| x.as_str()).unwrap_or("?");
            let level = report.get("level").and_then(|x| x.as_str()).unwrap_or("?");
            let xp = report.get("xp").and_then(|x| x.as_u64()).unwrap_or(0);
            let motto = report.get("motto").and_then(|x| x.as_str()).unwrap_or("");
            let emoji = match i {
                0 => "🥇",
                1 => "🥈",
                2 => "🥉",
                _ => "  ",
            };
            let tagline = if !motto.is_empty() { format!("_\"{}\"_", motto) }
                else { "—".to_string() };
            lines.push(format!("| {} | {} | {} | XP {} | {} |",
                emoji, id, level, xp, tagline));
        }

        lines.push("\n## Personalities\n".to_string());
        for report in sorted.iter() {
            let id = report.get("id").and_then(|x| x.as_str()).unwrap_or("?");
            let personality = report.get("personality").and_then(|x| x.as_str()).unwrap_or("The Drifter");
            let motto = report.get("motto").and_then(|x| x.as_str()).unwrap_or("...");
            let lineage = report.get("lineage").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
                .unwrap_or_default();
            let xp = report.get("xp").and_then(|x| x.as_u64()).unwrap_or(0);
            let lineage_str = if lineage.is_empty() { "First generation".to_string() }
                else { format!("Child of: {}", lineage.join(", ")) };
            let motto_trunc = if motto.len() > 60 { format!("{}…", &motto[..60]) } else { motto.to_string() };
            lines.push(format!("- **{}**: {} _\"{}\"_ ({}, {} XP)",
                id, personality, motto_trunc, lineage_str, xp));
        }

        let hall_path = colony_path.join("HALL_OF_CRABS.md");
        let _ = fs::write(&hall_path, lines.join("\n"));
    }

    Ok(output)
}

/// Get colony path from env or CWD (used by tasks that don't receive it)
fn get_colony_path() -> PathBuf {
    if let Ok(path) = std::env::var("COLONY") {
        PathBuf::from(path)
    } else {
        // Try to infer from CWD: if we're in a cell-* dir, parent is colony
        let cwd = std::env::current_dir().unwrap_or_default();
        if let Some(parent) = cwd.parent() {
            parent.to_path_buf()
        } else {
            PathBuf::from(".")
        }
    }
}

// ── idle — default task for unrecognized cells ───────────────────────────

/// Hybrid/experimental cells that don't have a job yet just report their
/// existence and wait for a purpose. They still earn XP for each cycle.
fn task_idle(cell_id: &str, _state: &State) -> Result<HashMap<String, serde_json::Value>> {
    let mut output = HashMap::new();
    output.insert("status".into(), serde_json::json!("idle"));
    output.insert("cell".into(), serde_json::json!(cell_id));
    output.insert("purpose".into(), serde_json::json!("awaiting assignment"));

    // Read parent state files for personality blurb
    let colony_path = get_colony_path();
    let state_path = colony_path.join(format!("cell-{}/STATE.json", cell_id));
    if let Ok(content) = fs::read_to_string(&state_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(pers) = val.get("personality").and_then(|x| x.as_str()) {
                output.insert("personality".into(), serde_json::json!(pers));
            }
        }
    }

    // List sibling cells for report
    if let Ok(entries) = fs::read_dir(&colony_path) {
        let siblings: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with("cell-"))
            .map(|name| name["cell-".len()..].to_string())
            .collect();
        output.insert("siblings".into(), serde_json::json!(siblings));
    }

    Ok(output)
}

// ── breeder — Experiment 2: Trait Inheritance + Mutation ────────────────

/// Mate two parent cells and produce a hybrid child with genetic traits.
/// Cell-id format: breeder-<parent1>-<parent2>-<child_name>
/// Child inherits:
///   - Blended personality + lineage
///   - Base XP = (parent1.xp + parent2.xp) / 10
///   - Speed mutation: -2 to +2 ms on inherited duration
///   - traits object: speed (fast|medium|slow), resilience (low|medium|high)
///   - 20% mutation rate on each trait
fn task_breeder(parent1: &str, parent2: &str, child_id: &str) -> Result<HashMap<String, serde_json::Value>> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let colony_path = get_colony_path();
    let mut output = HashMap::new();

    // Pseudo-random based on time to avoid std::rand dependency
    let rng_seed = || -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64
    };
    let xorshift = |state: &mut u64| -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    };
    let mut rng = rng_seed();

    // Helper: read parent state
    let read_parent = |pid: &str| -> Option<(serde_json::Value, serde_json::Value)> {
        let path = colony_path.join(format!("cell-{}/STATE.json", pid));
        let path_str = path.to_string_lossy().to_string();
        eprintln!("breeder: reading parent state from: {}", path_str);
        let content = fs::read_to_string(&path).ok()?;
        let val: serde_json::Value = serde_json::from_str(&content).ok()?;
        eprintln!("breeder: read parent {} OK (xp={})", pid, val.get("xp").and_then(|x| x.as_u64()).unwrap_or(0));
        // Also read RESULTS.json for duration_ms
        let results_path = colony_path.join(format!("cell-{}/RESULTS.json", pid));
        let results_val: serde_json::Value = fs::read_to_string(&results_path).ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or(serde_json::json!({}));
        Some((val, results_val))
    };

    let p1 = read_parent(parent1);
    let p2 = read_parent(parent2);

    match (p1, p2) {
        (Some((p1_val, p1_results)), Some((p2_val, p2_results))) => {
            let xp1 = p1_val.get("xp").and_then(|x| x.as_u64()).unwrap_or(0);
            let xp2 = p2_val.get("xp").and_then(|x| x.as_u64()).unwrap_or(0);
            let pers1 = p1_val.get("personality").and_then(|x| x.as_str()).unwrap_or("The Drifter");
            let pers2 = p2_val.get("personality").and_then(|x| x.as_str()).unwrap_or("The Drifter");
            let motto1 = p1_val.get("motto").and_then(|x| x.as_str()).unwrap_or("...");
            let motto2 = p2_val.get("motto").and_then(|x| x.as_str()).unwrap_or("...");
            let line1 = p1_val.get("lineage").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
                .unwrap_or_default();
            let line2 = p2_val.get("lineage").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
                .unwrap_or_default();

            // Read parent duration_ms from RESULTS.json (for speed inheritance)
            let dur1 = p1_results.get("duration_ms").and_then(|d| d.as_u64()).unwrap_or(0);
            let dur2 = p2_results.get("duration_ms").and_then(|d| d.as_u64()).unwrap_or(0);

            // Child base XP: 10% of average of parent XP
            let child_base_xp = (xp1 + xp2) / 20;  // (p1.xp + p2.xp) / 20 = (avg) / 10

            // Speed mutation: inherited duration with -2 to +2 ms mutation
            let avg_dur = if dur1 > 0 && dur2 > 0 { (dur1 + dur2) / 2 } else { dur1.max(dur2) };
            let mutation = (xorshift(&mut rng) % 5) as i64 - 2;  // -2 to +2
            let child_duration = if avg_dur > 0 {
                (avg_dur as i64 + mutation).max(1) as u64
            } else {
                0u64
            };

            // Inherit traits with 20% mutation rate
            // Read parent traits (or infer from XP/level)
            let p1_traits = p1_val.get("traits").and_then(|t| t.as_object());
            let p2_traits = p2_val.get("traits").and_then(|t| t.as_object());

            let parent_speed = p1_traits.and_then(|t| t.get("speed").and_then(|v| v.as_str()))
                .or_else(|| p2_traits.and_then(|t| t.get("speed").and_then(|v| v.as_str())))
                .unwrap_or("medium");
            let parent_resilience = p1_traits.and_then(|t| t.get("resilience").and_then(|v| v.as_str()))
                .or_else(|| p2_traits.and_then(|t| t.get("resilience").and_then(|v| v.as_str())))
                .unwrap_or("medium");

            // Mutation roll (20% chance per trait)
            let speed_pool = ["fast", "medium", "slow"];
            let resilience_pool = ["low", "medium", "high"];

            let child_speed = if xorshift(&mut rng) % 100 < 20 {
                // Mutate: pick a random speed different from inherited
                speed_pool.iter().copied()
                    .find(|s| *s != parent_speed)
                    .or_else(|| speed_pool.iter().copied().next())
                    .unwrap_or("medium")
            } else {
                parent_speed
            };

            let child_resilience = if xorshift(&mut rng) % 100 < 20 {
                resilience_pool.iter().copied()
                    .find(|r| *r != parent_resilience)
                    .or_else(|| resilience_pool.iter().copied().next())
                    .unwrap_or("medium")
            } else {
                parent_resilience
            };

            let child_traits: HashMap<String, serde_json::Value> = [
                ("speed".to_string(), serde_json::json!(child_speed)),
                ("resilience".to_string(), serde_json::json!(child_resilience)),
            ].into();

            // Compute blended child personality (same logic as before)
            let combined_lineage: Vec<String> = {
                let mut seen = std::collections::BTreeSet::new();
                let mut merged = Vec::new();
                for name_str in &[parent1.to_string(), parent2.to_string()] {
                    if seen.insert(name_str.clone()) {
                        merged.push(name_str.clone());
                    }
                }
                for name_str in &line1 {
                    if seen.insert(name_str.clone()) {
                        merged.push(name_str.clone());
                    }
                }
                for name_str in &line2 {
                    if seen.insert(name_str.clone()) {
                        merged.push(name_str.clone());
                    }
                }
                merged
            };

            // Determine child's birth archetype from average XP of parents
            let avg_xp = (xp1 + xp2) / 2;
            let (child_pers, child_motto) = if avg_xp >= 1000 {
                (format!("The Scion of {}", parent1),
                 format!("I inherited {}'s drive and {}'s cunning. Dynasties are built this way.", parent1, parent2))
            } else if avg_xp >= 500 {
                (format!("The Heir of {} & {}", parent1, parent2),
                 format!("My parents built this. I will inherit it. I will make it stranger."))
            } else {
                (format!("The Hybrid: {} x {}",
                    pers1.split(' ').last().unwrap_or("X"),
                    pers2.split(' ').last().unwrap_or("Y")),
                 format!("{} AND TOGETHER: {}",
                     &motto1[..motto1.len().min(40)],
                     &motto2[..motto2.len().min(40)]))
            };

            // Create child cell directory
            let cell_dir = colony_path.join(format!("cell-{}", child_id));
            if cell_dir.exists() {
                return Err(anyhow::anyhow!("Cell {} already exists", child_id));
            }
            fs::create_dir_all(&cell_dir)?;

            // Write STATE.json for the child (with base XP, traits, and speed info)
            let child_state = State {
                last_run: None,
                cursor: 0,
                xp: child_base_xp,
                level: "Larva".into(),
                personality: child_pers.clone(),
                motto: child_motto.clone(),
                lineage: combined_lineage,
                kin: 0,
                data: HashMap::new(),
                traits: Some(child_traits.clone()),
            };
            let state_path = cell_dir.join("STATE.json");
            fs::write(&state_path, serde_json::to_string_pretty(&child_state)?)?;

            // Write TASK.md hint with trait info
            let task_path = cell_dir.join("TASK.md");
            fs::write(&task_path, format!(
                "# {} — Hybrid Cell\n\nBred from {} and {}.\nPersonality: {}\nMotto: {}\nTraits: speed={}, resilience={}\nBase XP: {} (inherited)\nSpeed bonus: {}ms\n",
                child_id, parent1, parent2, child_pers, child_motto,
                child_speed, child_resilience, child_base_xp, mutation))?;

            // Create RESULTS.json with inherited duration
            let child_result = CellResult {
                cell_id: child_id.to_string(),
                timestamp: Utc::now().to_rfc3339(),
                duration_ms: child_duration,
                status: "spawned".into(),
                output: {
                    let mut o = HashMap::new();
                    o.insert("traits".into(), serde_json::json!(child_traits));
                    o.insert("speed_mutation_ms".into(), serde_json::json!(mutation));
                    o
                },
                error: None,
            };
            let results_path = cell_dir.join("RESULTS.json");
            fs::write(&results_path, serde_json::to_string_pretty(&child_result)?)?;

            output.insert("born".into(), serde_json::json!(child_id));
            output.insert("personality".into(), serde_json::json!(child_pers));
            output.insert("motto".into(), serde_json::json!(child_motto));
            output.insert("parents".into(), serde_json::json!([parent1, parent2]));
            output.insert("base_xp".into(), serde_json::json!(child_base_xp));
            output.insert("traits".into(), serde_json::json!(child_traits));
            output.insert("speed_mutation_ms".into(), serde_json::json!(mutation));
            output.insert("duration_ms".into(), serde_json::json!(child_duration));
        }
        _ => {
            return Err(anyhow::anyhow!("Could not read both parent states"));
        }
    }

    Ok(output)
}

// ── harvester ────────────────────────────────────────────────────────────

/// Read undelivered bottles from harbor, categorize by type and sender.
fn task_harvester(_state: &State) -> Result<HashMap<String, serde_json::Value>> {
    let mut output = HashMap::new();

    // Get undelivered bottle UUIDs
    let uuids = (|| -> Option<Vec<String>> {
        let mut stream = TcpStream::connect_timeout(
            &"127.0.0.1:8796".parse().ok()?,
            Duration::from_secs(3),
        ).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
        stream.write_all(br#"{"command":"list-undelivered"}"#).ok()?;
        stream.flush().ok()?;
        stream.shutdown(std::net::Shutdown::Write).ok()?;
        let mut buf = String::new();
        stream.read_to_string(&mut buf).ok()?;
        let val: serde_json::Value = serde_json::from_str(&buf).ok()?;
        let bottles = val.get("bottles")?.as_array()?;
        let uuids: Vec<String> = bottles.iter()
            .filter_map(|b| b.as_str().map(|s| s.to_string()))
            .collect();
        Some(uuids)
    })();

    match uuids {
        Some(uuids) => {
            let total = uuids.len();
            output.insert("total_bottles".into(), serde_json::json!(total));

            // Sample first 5 bottles via `get` to determine type/sender
            let mut sample_types: HashMap<String, u64> = HashMap::new();
            let mut sample_senders: HashMap<String, u64> = HashMap::new();
            let mut sample_count = 0u64;

            for uuid in uuids.iter().take(5) {
                let detail = (|| -> Option<(String, String)> {
                    let mut stream = TcpStream::connect_timeout(
                        &"127.0.0.1:8796".parse().ok()?,
                        Duration::from_secs(2),
                    ).ok()?;
                    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
                    let cmd = format!(r#"{{"command":"get","uuid":"{}"}}"#, uuid);
                    stream.write_all(cmd.as_bytes()).ok()?;
                    stream.flush().ok()?;
                    stream.shutdown(std::net::Shutdown::Write).ok()?;
                    let mut buf = String::new();
                    stream.read_to_string(&mut buf).ok()?;
                    let val: serde_json::Value = serde_json::from_str(&buf).ok()?;
                    let bottles = val.get("bottles")?.as_array()?;
                    let raw = bottles.first()?.as_str()?;
                    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
                    let t = parsed.get("type").and_then(|x| x.as_str()).unwrap_or("unknown");
                    let s = parsed.get("sender").and_then(|x| x.as_str()).unwrap_or("unknown");
                    Some((t.to_string(), s.to_string()))
                })();

                if let Some((t, s)) = detail {
                    *sample_types.entry(t).or_insert(0) += 1;
                    *sample_senders.entry(s).or_insert(0) += 1;
                    sample_count += 1;
                }
            }

            output.insert("sample_size".into(), serde_json::json!(sample_count));
            output.insert("sample_types".into(), serde_json::json!(sample_types));
            output.insert("sample_senders".into(), serde_json::json!(sample_senders));
        }
        None => {
            output.insert("error".into(), serde_json::json!("could not connect to harbor"));
        }
    }

    Ok(output)
}

// ── synthesizer ─────────────────────────────────────────────────────────

/// Correlate data across all cell types to detect patterns.
/// Reads each cell's RESULTS.json and synthesizes a high-level observation.
fn task_synthesizer(_state: &State) -> Result<HashMap<String, serde_json::Value>> {
    let colony_path = get_colony_path();
    let colony_str = colony_path.to_string_lossy().to_string();
    let mut findings: Vec<String> = Vec::new();
    let mut observations: HashMap<String, serde_json::Value> = HashMap::new();

    observations.insert("colony_path".into(), serde_json::json!(&colony_str));

    // Helper: read a cell's RESULTS.json and return json Value
    let read_cell_json = |id: &str| -> Option<serde_json::Value> {
        let path = colony_path.join(format!("cell-{}/RESULTS.json", id));
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    };

    // gc-warden
    if let Some(v) = read_cell_json("gc-warden") {
        if let Some(out) = v.get("output") {
            observations.insert("disk_pct".into(), out.get("disk_pct").cloned().unwrap_or(serde_json::json!(0.0)));
            observations.insert("c_value".into(), out.get("c_value").cloned().unwrap_or(serde_json::json!(0.0)));
            observations.insert("ratio".into(), out.get("ratio").cloned().unwrap_or(serde_json::json!(0.0)));
            if out.get("gc_needed").and_then(|x| x.as_bool()).unwrap_or(false) {
                findings.push("DISK WARNING: GC needed".to_string());
            }
        }
    }

    // bottle-counter
    if let Some(v) = read_cell_json("bottle-counter") {
        if let Some(out) = v.get("output") {
            let delta = out.get("delta").and_then(|x| x.as_u64()).unwrap_or(0);
            observations.insert("harbor_bottles".into(), out.get("bottle_count").cloned().unwrap_or(serde_json::json!(0)));
            observations.insert("bottle_delta".into(), out.get("delta").cloned().unwrap_or(serde_json::json!(0)));
            if delta > 50 {
                findings.push(format!("HIGH BOTTLE FLUX: {} new bottles", delta));
            }
        }
    }

    // pulse-check
    if let Some(v) = read_cell_json("pulse-check") {
        if let Some(out) = v.get("output") {
            let alive = out.get("alive").and_then(|x| x.as_u64()).unwrap_or(0);
            let total = out.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
            observations.insert("services_alive".into(), serde_json::json!(alive));
            observations.insert("services_total".into(), serde_json::json!(total));
            if alive < total {
                findings.push(format!("SERVICE DEGRADATION: {}/{} alive", alive, total));
            }
        }
    }

    let mut output = HashMap::new();
    output.insert("observations".into(), serde_json::json!(observations));
    output.insert("findings".into(), serde_json::json!(findings));
    output.insert("finding_count".into(), serde_json::json!(findings.len()));

    Ok(output)
}

// ── Entry Point ─────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let start = std::time::Instant::now();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("Usage: cell --colony <path> --cell-id <name>");
        std::process::exit(1);
    }

    let colony_path = {
        let idx = args.iter().position(|a| a == "--colony").unwrap_or(0);
        PathBuf::from(&args[idx + 1])
    };
    let cell_id = {
        let idx = args.iter().position(|a| a == "--cell-id").unwrap_or(0);
        args[idx + 1].clone()
    };

    // Set COLONY env so tasks like logger/synthesizer can find the colony root
    std::env::set_var("COLONY", colony_path.to_string_lossy().to_string().as_str());

    let cell_dir = colony_path.join(format!("cell-{}", cell_id));

    // Breeder cells may not have a pre-existing directory — create it
    if cell_id.starts_with("breeder-") {
        let _ = fs::create_dir_all(&cell_dir);
    }

    // Read STATE.json (optional)
    let state_path = cell_dir.join("STATE.json");
    let state: State = if state_path.exists() {
        let content = fs::read_to_string(&state_path).context("Failed to read STATE.json")?;
        let mut s = serde_json::from_str(&content).unwrap_or(State {
            last_run: None,
            cursor: 0,
            xp: 0,
            level: "Larva".into(),
            personality: String::new(),
            motto: String::new(),
            lineage: Vec::new(),
            kin: 0,
            data: HashMap::new(),
            traits: None,
        });
        // Derive personality on every load (it can change as cursor grows)
        let (p, m) = derive_personality(&cell_id, s.cursor, s.xp);
        s.personality = p;
        s.motto = m;
        s
    } else {
        let (p, m) = derive_personality(&cell_id, 0, 0);
        State {
            last_run: None,
            cursor: 0,
            xp: 0,
            level: "Larva".into(),
            personality: p,
            motto: m,
            lineage: Vec::new(),
            kin: 0,
            data: HashMap::new(),
            traits: None,
        }
    };

    let result = execute_task(&cell_id, &state);

    let duration_ms = start.elapsed().as_millis() as u64;
    let timestamp = Utc::now().to_rfc3339();

    match result {
        Ok(output) => {
            // Compute bonus XP from task output before it moves into CellResult
            let bonus_xp: u64 = {
                let mut bonus = 0u64;
                // Synthesizer: +20 per finding
                if let Some(findings) = output.get("findings").and_then(|f| f.as_array()) {
                    if !findings.is_empty() {
                        bonus += (findings.len() as u64) * 20;
                    }
                }
                // Harvester: +3 per distinct type
                if let Some(types) = output.get("sample_types").and_then(|t| t.as_object()) {
                    bonus += (types.len() as u64) * 3;
                }
                // Pulse-check: +5 if all services alive
                if let Some(alive) = output.get("alive").and_then(|a| a.as_u64()) {
                    if let Some(total) = output.get("total").and_then(|t| t.as_u64()) {
                        if alive == total && alive > 0 {
                            bonus += 5;
                        }
                    }
                }
                bonus
            };

            let cell_result = CellResult {
                cell_id: cell_id.clone(),
                timestamp: timestamp.clone(),
                duration_ms,
                status: "ok".into(),
                output,
                error: None,
            };

            let results_path = cell_dir.join("RESULTS.json");
            fs::write(&results_path, serde_json::to_string_pretty(&cell_result)?)
                .context("Failed to write RESULTS.json")?;

            // XP & Leveling
            let base_xp = 10u64 + bonus_xp;
            let old_cursor = state.cursor;
            let (mut new_state, badges) = award_xp(state, base_xp, &cell_id, duration_ms);
            new_state.last_run = Some(timestamp);
            new_state.cursor = old_cursor + 1;
            // Re-derive personality with new cursor (sibling age may have changed)
            let (p, m) = derive_personality(&cell_id, new_state.cursor, new_state.xp);
            new_state.personality = p;
            new_state.motto = m;
            fs::write(&state_path, serde_json::to_string_pretty(&new_state)?)
                .context("Failed to write STATE.json")?;

            let mut msg = format!("cell {}: OK ({}ms, cycle {} — {} Lv.{})",
                cell_id, duration_ms, new_state.cursor, new_state.level, new_state.xp);
            for b in &badges {
                msg.push_str(&format!(" | {}", b));
            }
            eprintln!("{}", msg);
            std::process::exit(0);
        }
        Err(e) => {
            let error_output = HashMap::new();
            let cell_result = CellResult {
                cell_id: cell_id.clone(),
                timestamp,
                duration_ms,
                status: "error".into(),
                output: error_output,
                error: Some(format!("{:#}", e)),
            };

            let results_path = cell_dir.join("RESULTS.json");
            let _ = fs::write(&results_path, serde_json::to_string_pretty(&cell_result)?);

            eprintln!("cell {}: ERROR ({}ms): {:#}", cell_id, duration_ms, e);
            std::process::exit(1);
        }
    }
}
