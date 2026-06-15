#!/usr/bin/env python3
"""
colony-api.py — Colony REST API

Endpoints:
  GET  /api/status       — JSON with all cells' current state
  GET  /api/hall         — HALL_OF_CRABS.md parsed as JSON
  GET  /api/breed?p1=X&p2=Y&child=Z  — Spawn breeder run, return child info
  POST /api/cull        — Trigger culler

Usage: python3 colony-api.py [--port 8820] [--colony <path>]
"""

import json
import os
import re
import subprocess
import sys
import time
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs

COLONY = os.environ.get("COLONY", os.path.dirname(os.path.abspath(__file__)))
CELL_BINARY = os.path.join(COLONY, "cell", "target", "release", "cell")
PORT = int(os.environ.get("API_PORT", 8820))


def get_colony_path():
    return COLONY


def read_cell_states():
    """Read all cell STATE.json files and return a list."""
    colony = get_colony_path()
    cells = []
    if not os.path.isdir(colony):
        return {"error": f"Colony path not found: {colony}"}

    for entry in sorted(os.listdir(colony)):
        if entry.startswith("cell-"):
            cell_id = entry[5:]  # strip "cell-" prefix
            state_path = os.path.join(colony, entry, "STATE.json")
            results_path = os.path.join(colony, entry, "RESULTS.json")
            cell_info = {"id": cell_id, "dir": entry}
            if os.path.isfile(state_path):
                try:
                    with open(state_path) as f:
                        cell_info["state"] = json.load(f)
                except (json.JSONDecodeError, OSError) as e:
                    cell_info["state_error"] = str(e)
            if os.path.isfile(results_path):
                try:
                    with open(results_path) as f:
                        cell_info["result"] = json.load(f)
                except (json.JSONDecodeError, OSError) as e:
                    cell_info["result_error"] = str(e)
            cells.append(cell_info)

    return cells


def parse_hall_of_crabs():
    """Parse HALL_OF_CRABS.md into structured JSON."""
    hall_path = os.path.join(get_colony_path(), "HALL_OF_CRABS.md")
    if not os.path.isfile(hall_path):
        return {"error": "HALL_OF_CRABS.md not found"}

    with open(hall_path) as f:
        content = f.read()

    lines = content.split("\n")

    # Extract generated timestamp
    generated = ""
    for line in lines:
        m = re.search(r"\*Generated (.+?) UTC", line)
        if m:
            generated = m.group(1)
            break

    # Parse ranked table
    rankings = []
    in_table = False
    for line in lines:
        if line.startswith("| Rank |"):
            in_table = True
            continue
        if line.startswith("|------|"):
            continue
        if in_table and line.startswith("|"):
            parts = [p.strip() for p in line.split("|")]
            if len(parts) >= 6:
                rank_emoji = parts[1].strip()
                cell_id = parts[2]
                level = parts[3]
                xp_str = parts[4].strip()
                xp = 0
                if xp_str.startswith("XP "):
                    xp = int(xp_str[3:])
                tagline = parts[5].strip().strip("_\"'")
                rankings.append({
                    "rank": rank_emoji if rank_emoji else len(rankings) + 1,
                    "cell_id": cell_id,
                    "level": level,
                    "xp": xp,
                    "tagline": tagline,
                })
        elif in_table and not line.startswith("|"):
            break

    # Parse personalities
    personalities = []
    in_pers = False
    for line in lines:
        if line.strip().startswith("## Personalities"):
            in_pers = True
            continue
        if in_pers and line.startswith("- **"):
            m = re.match(r"- \*\*([^*]+)\*\*: (.+)", line)
            if m:
                pers_raw = m.group(2)
                personalities.append({
                    "cell_id": m.group(1),
                    "description": pers_raw,
                })
        elif in_pers and line.strip() == "":
            continue

    return {
        "generated": generated,
        "cell_count": len(rankings),
        "rankings": rankings,
        "personalities": personalities,
        "raw_markdown": content,
    }


class ColonyAPIHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path
        params = parse_qs(parsed.query)

        if path == "/api/status":
            self.send_json(read_cell_states())

        elif path == "/api/hall":
            self.send_json(parse_hall_of_crabs())

        elif path == "/api/breed":
            p1 = params.get("p1", [None])[0]
            p2 = params.get("p2", [None])[0]
            child = params.get("child", [None])[0]

            if not p1 or not p2:
                self.send_json({"error": "Missing p1 and/or p2 query params"}, 400)
                return

            if not child:
                child = f"{p1}x{p2}x{int(time.time())}"

            breeder_id = f"breeder-{p1}x{p2}x{child}"

            try:
                result = subprocess.run(
                    [CELL_BINARY, "--colony", COLONY, "--cell-id", breeder_id],
                    capture_output=True, text=True, timeout=30,
                    cwd=COLONY,
                )

                # Read child state if it was created
                child_state_path = os.path.join(COLONY, f"cell-{child}", "STATE.json")
                child_info = None
                if os.path.isfile(child_state_path):
                    with open(child_state_path) as f:
                        child_info = json.load(f)

                self.send_json({
                    "breeder_id": breeder_id,
                    "parent1": p1,
                    "parent2": p2,
                    "child": child,
                    "status": "ok" if result.returncode == 0 else "error",
                    "stderr": result.stderr,
                    "stdout": result.stdout,
                    "child_state": child_info,
                })
            except subprocess.TimeoutExpired:
                self.send_json({"error": "Breeder timed out"}, 500)
            except Exception as e:
                self.send_json({"error": str(e)}, 500)

        elif path == "/api/cull":
            try:
                result = subprocess.run(
                    [CELL_BINARY, "--colony", COLONY, "--cell-id", "culler"],
                    capture_output=True, text=True, timeout=30,
                    cwd=COLONY,
                )

                # Read culler results
                culler_results_path = os.path.join(COLONY, "cell-culler", "RESULTS.json")
                cull_output = None
                if os.path.isfile(culler_results_path):
                    with open(culler_results_path) as f:
                        cull_output = json.load(f)

                self.send_json({
                    "status": "ok" if result.returncode == 0 else "error",
                    "stderr": result.stderr,
                    "stdout": result.stdout,
                    "cull_output": cull_output,
                })
            except subprocess.TimeoutExpired:
                self.send_json({"error": "Culler timed out"}, 500)
            except Exception as e:
                self.send_json({"error": str(e)}, 500)

        elif path == "/api/run":
            cell_id = params.get("id", [None])[0]
            if not cell_id:
                self.send_json({"error": "Missing id query param"}, 400)
                return

            try:
                result = subprocess.run(
                    [CELL_BINARY, "--colony", COLONY, "--cell-id", cell_id],
                    capture_output=True, text=True, timeout=30,
                    cwd=COLONY,
                )

                # Read updated state
                state_path = os.path.join(COLONY, f"cell-{cell_id}", "STATE.json")
                new_state = None
                if os.path.isfile(state_path):
                    with open(state_path) as f:
                        new_state = json.load(f)

                self.send_json({
                    "cell_id": cell_id,
                    "status": "ok" if result.returncode == 0 else "error",
                    "stderr": result.stderr,
                    "stdout": result.stdout,
                    "new_state": new_state,
                })
            except subprocess.TimeoutExpired:
                self.send_json({"error": "Cell run timed out"}, 500)
            except Exception as e:
                self.send_json({"error": str(e)}, 500)

        else:
            self.send_json({"error": f"Not found: {path}"}, 404)

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length).decode() if content_length > 0 else ""
        parsed_path = urlparse(self.path)
        path = parsed_path.path

        if path == "/api/cull":
            # Same as GET but POST
            try:
                result = subprocess.run(
                    [CELL_BINARY, "--colony", COLONY, "--cell-id", "culler"],
                    capture_output=True, text=True, timeout=30,
                    cwd=COLONY,
                )
                culler_results_path = os.path.join(COLONY, "cell-culler", "RESULTS.json")
                cull_output = None
                if os.path.isfile(culler_results_path):
                    with open(culler_results_path) as f:
                        cull_output = json.load(f)
                self.send_json({
                    "status": "ok" if result.returncode == 0 else "error",
                    "stderr": result.stderr,
                    "stdout": result.stdout,
                    "cull_output": cull_output,
                })
            except subprocess.TimeoutExpired:
                self.send_json({"error": "Culler timed out"}, 500)
            except Exception as e:
                self.send_json({"error": str(e)}, 500)

        elif path == "/api/breed":
            # Accept JSON body or query params
            try:
                data = json.loads(body) if body else {}
            except json.JSONDecodeError:
                data = {}
            p1 = data.get("p1")
            p2 = data.get("p2")
            child = data.get("child")
            if not p1 or not p2:
                self.send_json({"error": "Missing p1 and/or p2 in JSON body"}, 400)
                return
            if not child:
                child = f"{p1}x{p2}x{int(time.time())}"
            breeder_id = f"breeder-{p1}x{p2}x{child}"
            try:
                result = subprocess.run(
                    [CELL_BINARY, "--colony", COLONY, "--cell-id", breeder_id],
                    capture_output=True, text=True, timeout=30,
                    cwd=COLONY,
                )
                child_state_path = os.path.join(COLONY, f"cell-{child}", "STATE.json")
                child_info = None
                if os.path.isfile(child_state_path):
                    with open(child_state_path) as f:
                        child_info = json.load(f)
                self.send_json({
                    "breeder_id": breeder_id,
                    "parent1": p1,
                    "parent2": p2,
                    "child": child,
                    "status": "ok" if result.returncode == 0 else "error",
                    "stderr": result.stderr,
                    "stdout": result.stdout,
                    "child_state": child_info,
                })
            except subprocess.TimeoutExpired:
                self.send_json({"error": "Breeder timed out"}, 500)
            except Exception as e:
                self.send_json({"error": str(e)}, 500)

        else:
            self.send_json({"error": f"Not found: {path}"}, 404)

    def send_json(self, data, status=200):
        body = json.dumps(data, indent=2, default=str).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        stderr_print(f"[API] {args[0]} {args[1]} {args[2]}")


def stderr_print(*args, **kwargs):
    """Print to stderr so stdout isn't polluted."""
    print(*args, **kwargs, file=sys.stderr, flush=True)


def main():
    colony = get_colony_path()
    stderr_print(f"Colony API starting on port {PORT}")
    stderr_print(f"Colony path: {colony}")
    stderr_print(f"Cell binary: {CELL_BINARY}")

    # Ensure culler cell directory exists
    culler_dir = os.path.join(colony, "cell-culler")
    if not os.path.isdir(culler_dir):
        os.makedirs(culler_dir, exist_ok=True)
        culler_state = {
            "last_run": None,
            "cursor": 0,
            "xp": 0,
            "level": "Larva",
            "personality": "The Culler",
            "motto": "I cull the weak hybrids.",
            "lineage": [],
            "kin": 0,
            "data": {},
        }
        with open(os.path.join(culler_dir, "STATE.json"), "w") as f:
            json.dump(culler_state, f, indent=2)
        stderr_print("Created culler cell directory with STATE.json")

    server = HTTPServer(("0.0.0.0", PORT), ColonyAPIHandler)
    stderr_print(f"Listening on http://0.0.0.0:{PORT}")
    stderr_print("Endpoints: GET /api/status, GET /api/hall, GET/POST /api/breed, GET/POST /api/cull")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        stderr_print("\nShutting down...")
        server.server_close()


if __name__ == "__main__":
    main()
