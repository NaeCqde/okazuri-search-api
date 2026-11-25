#!/usr/bin/env python3
"""Adds up what the Claude steps of GitHub Actions cost, over all the repositories of an owner.

It looks at every workflow that uses `anthropics/claude-code-action`, takes the runs created since a time, reads the log of each run and adds up the
`total_cost_usd`, `num_turns` and `duration_ms` that the action prints in its result. The cost is what the API would charge (the subscription has no price), so it is
a measure to compare with, not the share of the weekly limit: GitHub cannot tell the share, and neither does the usage card of Claude, which only has the total
of the account.

Needs the GitHub CLI (`gh`), logged in, and Python 3.

    python3 tools/claude-actions-usage.py                       # since the start of the weekly window (--window-reset), every repository of the logged in user
    python3 tools/claude-actions-usage.py --since 2026-11-22T03:00:00Z --owner NaeCqde
    python3 tools/claude-actions-usage.py --repos NaeCqde/heros NaeCqde/okazuri-search-api --json
"""

import argparse
import base64
import concurrent.futures as futures
import datetime as dt
import json
import re
import subprocess
import sys

ACTION = "anthropics/claude-code-action"
COST = re.compile(r'"total_cost_usd"\s*:\s*([0-9.eE+-]+)')
TURNS = re.compile(r'"num_turns"\s*:\s*(\d+)')
DURATION = re.compile(r'"duration_ms"\s*:\s*(\d+)')


def gh(*args, check=True):
    done = subprocess.run(["gh", *args], capture_output=True, text=True)
    if check and done.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args[:3])}: {done.stderr.strip()[:200]}")
    return done.stdout


def api(path):
    return json.loads(gh("api", "--paginate", "--slurp", path) or "[]")


def pages(path):
    """The items of a paginated list (`--slurp` gives a list of pages)."""
    out = []
    for page in api(path):
        out.extend(page if isinstance(page, list) else [page])
    return out


def repositories(owner):
    rows = json.loads(gh("repo", "list", owner, "--limit", "300", "--json", "nameWithOwner,isArchived,isFork"))
    return [row["nameWithOwner"] for row in rows if not row["isArchived"]]


def claude_workflows(repo):
    """The workflows of a repository whose file uses the Claude action: {workflow id: path}."""
    found = {}
    for workflow in api(f"repos/{repo}/actions/workflows")[0].get("workflows", []):
        if workflow.get("state") == "deleted":
            continue
        try:
            file = json.loads(gh("api", f"repos/{repo}/contents/{workflow['path']}"))
            text = base64.b64decode(file["content"]).decode("utf-8", "replace")
        except Exception:
            continue
        if ACTION in text:
            found[workflow["id"]] = workflow["path"]
    return found


def runs_since(repo, workflow_id, since):
    runs = []
    for page in api(f"repos/{repo}/actions/workflows/{workflow_id}/runs?per_page=100&created=%3E%3D{since}"):
        runs.extend(page.get("workflow_runs", []))
    return runs


def run_cost(repo, run_id):
    log = gh("run", "view", str(run_id), "-R", repo, "--log", check=False)
    return (
        sum(float(m) for m in COST.findall(log)),
        sum(int(m) for m in TURNS.findall(log)),
        sum(int(m) for m in DURATION.findall(log)) / 1000,
        len(COST.findall(log)),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--owner", help="whose repositories (default: the logged in user); private ones are included when the token of gh can read them")
    parser.add_argument("--orgs", action="store_true", help="also the repositories of the organizations that the user belongs to")
    parser.add_argument("--repos", nargs="*", help="only these repositories (owner/name)")
    parser.add_argument("--since", help="ISO time, e.g. 2026-11-22T03:00:00Z (default: 7 days ago)")
    parser.add_argument("--workers", type=int, default=8, help="logs read at the same time")
    parser.add_argument("--json", action="store_true", help="print JSON")
    args = parser.parse_args()

    since = args.since or (dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=7)).strftime("%Y-%m-%dT%H:%M:%SZ")
    owner = args.owner or gh("api", "user", "--jq", ".login").strip()
    owners = [owner] + (gh("api", "user/orgs", "--jq", ".[].login").split() if args.orgs else [])
    repos = args.repos or [repo for name in owners for repo in repositories(name)]

    jobs = []  # (repo, run id)
    per_repo = {}
    for repo in repos:
        try:
            workflows = claude_workflows(repo)
        except Exception as error:
            print(f"{repo}: {error}", file=sys.stderr)
            continue
        for workflow_id in workflows:
            for run in runs_since(repo, workflow_id, since):
                if run.get("conclusion") in ("success", "failure"):
                    jobs.append((repo, run["id"]))
        if workflows:
            per_repo[repo] = {"workflows": sorted(workflows.values()), "runs": 0, "claude_calls": 0, "cost_usd": 0.0, "turns": 0, "seconds": 0.0}

    with futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        for (repo, _), (cost, turns, seconds, calls) in zip(jobs, pool.map(lambda job: run_cost(*job), jobs)):
            row = per_repo[repo]
            row["runs"] += 1
            row["claude_calls"] += calls
            row["cost_usd"] += cost
            row["turns"] += turns
            row["seconds"] += seconds

    total = {key: sum(row[key] for row in per_repo.values()) for key in ("runs", "claude_calls", "cost_usd", "turns", "seconds")}

    if args.json:
        print(json.dumps({"since": since, "repositories": per_repo, "total": total}, indent=2))
        return

    print("Claude steps of GitHub Actions since " + since + " (" + ", ".join(owners) + ")\n")
    print(f"{'repository':42} {'runs':>5} {'calls':>6} {'turns':>6} {'minutes':>8} {'USD*':>8}")
    for repo, row in sorted(per_repo.items(), key=lambda item: -item[1]["cost_usd"]):
        print(f"{repo:42} {row['runs']:5} {row['claude_calls']:6} {row['turns']:6} {row['seconds'] / 60:8.1f} {row['cost_usd']:8.2f}")
    print(f"{'total':42} {total['runs']:5} {total['claude_calls']:6} {total['turns']:6} {total['seconds'] / 60:8.1f} {total['cost_usd']:8.2f}")
    print("\n* what the API would charge for the same use; the subscription has no price, so this is not a share of the weekly limit.")


if __name__ == "__main__":
    main()
