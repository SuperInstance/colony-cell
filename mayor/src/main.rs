//! mayor — colony governor
//!
//! Reads colony/manifest.toml, checks each cell's schedule against current time,
//! and spawns `cell --colony <path> --cell-id <name>` for due cells.
//!
//! Usage: mayor --colony <path> [--dry-run]
//!
//! Designed to run as a cron job itself (e.g., every minute).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

// ── Data Types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Manifest {
    colony_name: String,
    version: String,
    #[serde(default)]
    cell_binary: Option<String>,     // Path to cell binary, defaults to {colony}/cell/target/release/cell
    #[serde(default)]
    default_timeout_secs: Option<u64>,      // Default cell timeout (overridable per-cell)
    cells: Vec<CellDef>,
}

#[derive(Debug, Deserialize)]
struct CellDef {
    id: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_schedule")]
    schedule: String,                // Cron-like: "every 5min", "every 1h", "hourly", "daily"
    #[serde(default)]
    timeout_secs: Option<u64>,       // Per-cell timeout override
    description: Option<String>,
}

fn default_schedule() -> String {
    "every 10min".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
struct LastRun {
    runs: HashMap<String, String>,  // cell_id → ISO timestamp of last run
}

// ── Schedule Parser ─────────────────────────────────────────────────────

/// Given a schedule string (e.g. "every 5min", "hourly", "daily", "every 1h"),
/// return the interval in seconds. Returns None if we can't parse it.
fn parse_schedule_secs(schedule: &str) -> Option<u64> {
    let s = schedule.trim().to_lowercase();

    if s == "hourly" || s == "every 1h" || s == "every 60min" {
        return Some(3600);
    }
    if s == "daily" || s == "every 24h" || s == "every 1440min" || s == "every 1d" {
        return Some(86400);
    }
    if s == "weekly" || s == "every 7d" || s == "every 168h" {
        return Some(604800);
    }

    // Parse "every Nmin", "every Nm", "every Nh", "every Ns"
    if let Some(rest) = s.strip_prefix("every ") {
        let rest = rest.trim();
        if let Some(num_str) = rest.split(|c: char| !c.is_ascii_digit()).next() {
            if let Ok(num) = num_str.parse::<u64>() {
                if rest.contains("h") {
                    return Some(num * 3600);
                } else if rest.contains("m") || rest.contains("min") {
                    return Some(num * 60);
                } else {
                    return Some(num); // assume seconds
                }
            }
        }
    }

    None
}

// ── Main ────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let mut colony_path = PathBuf::from(".");
    let mut dry_run = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--colony" => {
                i += 1;
                colony_path = PathBuf::from(&args[i]);
            }
            "--dry-run" => {
                dry_run = true;
            }
            _ => {}
        }
        i += 1;
    }

    // Read manifest
    let manifest_path = colony_path.join("manifest.toml");
    let manifest_content = fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read manifest at {}", manifest_path.display()))?;
    let manifest: Manifest = toml::from_str(&manifest_content)
        .context("Failed to parse manifest.toml")?;

    // Determine cell binary path
    let cell_binary = manifest.cell_binary.unwrap_or_else(|| {
        colony_path
            .join("cell/target/release/cell")
            .to_string_lossy()
            .to_string()
    });

    // Read last-run tracking (optional)
    let last_run_path = colony_path.join("mayor-last-run.json");
    let mut last_run: LastRun = if last_run_path.exists() {
        let content = fs::read_to_string(&last_run_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(LastRun {
            runs: HashMap::new(),
        })
    } else {
        LastRun {
            runs: HashMap::new(),
        }
    };

    let now = Utc::now();
    println!("{} | Colony '{}' — checking {} cells",
             now.format("%H:%M:%S"), manifest.colony_name, manifest.cells.len());

    let mut spawned = 0;
    let mut skipped = 0;
    let default_timeout = manifest.default_timeout_secs.unwrap_or(30);

    for cell in &manifest.cells {
        if !cell.enabled {
            println!("  {}: disabled, skipping", cell.id);
            skipped += 1;
            continue;
        }

        let last = last_run.runs.get(&cell.id);
        let interval_secs = parse_schedule_secs(&cell.schedule).unwrap_or(600); // default 10min

        let should_run = match last {
            None => true, // Never run before — due now
            Some(ts) => {
                if let Ok(last_dt) = ts.parse::<DateTime<Utc>>() {
                    let elapsed = (now - last_dt).num_seconds() as u64;
                    elapsed >= interval_secs
                } else {
                    true
                }
            }
        };

        if !should_run {
            println!("  {}: not due yet (schedule: {})", cell.id, cell.schedule);
            skipped += 1;
            continue;
        }

        let timeout = cell.timeout_secs.unwrap_or(default_timeout);
        let cell_dir = colony_path.join(format!("cell-{}", cell.id));

        // Ensure cell directory exists
        let cell_dir_str = cell_dir.to_string_lossy();
        let colony_str = colony_path.to_string_lossy();

        if dry_run {
            println!("  {}: WOULD RUN (timeout: {}s, dir: {})", cell.id, timeout, cell_dir_str);
        } else {
            println!("  {}: spawning (timeout: {}s)...", cell.id, timeout);

            let output = Command::new(&cell_binary)
                .args(["--colony", &colony_str, "--cell-id", &cell.id])
                .output()
                .with_context(|| format!("Failed to spawn cell {}", cell.id))?;

            let success = output.status.success();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if !stderr.is_empty() {
                print!("    stderr: {}", stderr.trim());
            }

            if success {
                println!("  {}: OK ✅", cell.id);
            } else {
                println!("  {}: FAILED ❌", cell.id);
            }
        }

        // Record run time
        last_run.runs.insert(cell.id.clone(), now.to_rfc3339());
        spawned += 1;
    }

    // Save last-run state
    if !dry_run {
        fs::write(&last_run_path, serde_json::to_string_pretty(&last_run)?)
            .context("Failed to write mayor-last-run.json")?;
    }

    println!("{} | Done: {} spawned, {} skipped", now.format("%H:%M:%S"), spawned, skipped);

    Ok(())
}
