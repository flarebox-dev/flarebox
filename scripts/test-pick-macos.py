#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/ci.yml"
JOBS = ("macos_app", "rust", "server", "spec", "runner_hook")
SELF_HOSTED = ["self-hosted", "custom-mac", "ARM64"]
XCODE = "/Applications/Xcode_27.2.app/Contents/Developer"
SELF_XCODE = "/Applications/Xcode.app/Contents/Developer"


def inline_script(job):
    body = WORKFLOW.read_text().split(f"\n  {job}:\n", 1)[1]
    body = re.split(r"\n  \S", body, maxsplit=1)[0]
    block = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", body)
    return textwrap.dedent(block[1])


def runner(busy=False, status="online", labels=SELF_HOSTED):
    return {"status": status, "busy": busy, "labels": [{"name": label} for label in labels]}


def hosted_job(*labels, status="in_progress"):
    return {"status": status, "labels": labels}


class Picker(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix=".test-pick-macos-", dir=ROOT)
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        self.write_executable("curl", """
            import json
            import os
            from pathlib import Path
            import sys
            url = sys.argv[-1].split('/actions/', 1)[1]
            with open(os.environ['REQUESTS'], 'a') as log:
                log.write(url + '\\n')
            fixtures = json.loads(Path(os.environ['FIXTURES']).read_text())
            if url not in fixtures:
                sys.exit('Unexpected API request: ' + url)
            if fixtures[url] is None:
                sys.exit('Fixture API failure: ' + url)
            print(json.dumps(fixtures[url]))
        """)

    def write_executable(self, name, source):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + textwrap.dedent(source))
        path.chmod(0o755)

    def run_script(self, script, env):
        output = self.dir / "output"
        output.write_text("")
        result = subprocess.run(
            ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script],
            cwd=self.dir,
            env={**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}",
                 "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(self.dir / "summary"),
                 "GITHUB_API_URL": "https://fixture.invalid", "GITHUB_TOKEN": "fixture-token",
                 "RUNNER_TEMP": str(self.dir), **env},
            capture_output=True, text=True, timeout=15,
        )
        return result, dict(line.split("=", 1) for line in output.read_text().splitlines())

    def pick(self, labels=("xcode-27",), wanted=JOBS, runners=(), jobs=(), env=None, fixtures=None):
        api = {
            "runners?per_page=100": {"runners": runners},
            "runs?status=queued&per_page=100&page=1": {"workflow_runs": [{"id": 1}]},
            "runs?status=in_progress&per_page=100&page=1": {"workflow_runs": [{"id": 1}]},
            "runs/1/jobs?filter=latest&per_page=100&page=1": {"jobs": jobs},
        }
        api.update(fixtures or {})
        (self.dir / "fixtures").write_text(json.dumps(api))
        (self.dir / "requests").write_text("")
        result, outputs = self.run_script(inline_script("pick-macos"), {
            "EVENT": "pull_request", "HEAD_REPO": "koidrive/koidrive", "REPO": "koidrive/koidrive",
            "RUNS_ON": json.dumps(labels), "RUNNER_STATUS_TOKEN": "fixture-status-token",
            "WANTED": " ".join(f"{job}={'true' if job in wanted else 'false'}" for job in JOBS),
            "REQUESTS": str(self.dir / "requests"), "FIXTURES": str(self.dir / "fixtures"),
            **(env or {}),
        })
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(set(outputs), set(JOBS))
        self.requests = (self.dir / "requests").read_text().splitlines()
        self.log = result.stdout
        return {job: json.loads(value) for job, value in outputs.items()}

    def assert_routes(self, outputs, self_hosted=(), labels=SELF_HOSTED):
        for job in JOBS:
            with self.subTest(job=job):
                expected = {"runs_on": labels, "developer_dir": SELF_XCODE} if job in self_hosted else {
                    "runs_on": ["xcode-27"] if job == "macos_app" else ["macos-15"],
                    "developer_dir": XCODE if job == "macos_app" else "",
                }
                self.assertEqual(outputs[job], expected)

    def test_hosted_labels_are_per_job_for_every_event(self):
        for event in ("pull_request", "merge_group", "push"):
            for labels in (["xcode-27"], ["macos-14", "extra-hosted-label"]):
                with self.subTest(event=event, labels=labels):
                    self.assert_routes(self.pick(labels=labels, env={"EVENT": event}))
                    self.assertEqual(self.requests, [])

    def test_forks_never_inspect_or_select_self_hosted_runners(self):
        for repo, head in (("koidrive/koidrive", "fork/repo"), ("koidrive/koidrive", ""),
                           ("fork/repo", "fork/repo")):
            with self.subTest(repo=repo, head=head):
                self.assert_routes(self.pick(labels=SELF_HOSTED, env={"REPO": repo, "HEAD_REPO": head}))
                self.assertEqual(self.requests, [])

    def test_fork_ignores_invalid_runner_configuration(self):
        self.assert_routes(self.pick(env={"HEAD_REPO": "fork/repo", "RUNS_ON": "not json"}))
        self.assertEqual(self.requests, [])

    def test_non_pr_self_hosted_preserves_arbitrary_labels_without_api(self):
        labels = ["custom-pool", "SELF-HOSTED", "ARM64"]
        for event in ("push", "merge_group"):
            with self.subTest(event=event):
                self.assert_routes(self.pick(labels=labels, env={"EVENT": event}), JOBS, labels)
                self.assertEqual(self.requests, [])

    def test_idle_runners_go_to_app_then_rust(self):
        outputs = self.pick(labels=SELF_HOSTED, runners=[runner(), runner(), runner(busy=True)])
        self.assert_routes(outputs, ("macos_app", "rust"))

    def test_skipped_jobs_do_not_consume_idle_or_hosted_capacity(self):
        outputs = self.pick(labels=SELF_HOSTED, wanted=("server", "spec", "runner_hook"), runners=[runner()])
        self.assert_routes(outputs, ("server",))
        self.assertIn("to_self_hosted=1 to_hosted=2", self.log)

    def test_enough_idle_runners_avoid_hosted_api(self):
        self.assert_routes(self.pick(labels=SELF_HOSTED, runners=[runner()] * 5), JOBS)
        self.assertEqual(self.requests, ["runners?per_page=100"])

    def test_shared_cap_counts_both_labels_once_and_ignores_self_hosted_and_completed(self):
        jobs = [hosted_job("macos-15"), hosted_job("MACOS-14"), hosted_job("xcode-27"),
                hosted_job("XCODE-27", "macos-15"), hosted_job("self-hosted", "macos-15"),
                hosted_job("SELF-HOSTED", "xcode-27"), hosted_job("macos-15", status="completed"),
                hosted_job("xcode-27", status="completed"), hosted_job("ubuntu-latest")]
        outputs = self.pick(labels=SELF_HOSTED, runners=[runner(busy=True)], jobs=jobs)
        self.assert_routes(outputs, ("rust", "server", "spec", "runner_hook"))
        self.assertIn("hosted_load=4 hosted_cap=5 to_self_hosted=4 to_hosted=1", self.log)
        self.assertEqual(self.requests.count("runs/1/jobs?filter=latest&per_page=100&page=1"), 1)

    def test_full_shared_cap_queues_for_self_hosted(self):
        jobs = [hosted_job("macos-15")] * 3 + [hosted_job("xcode-27")] * 2
        self.assert_routes(self.pick(labels=SELF_HOSTED, runners=[runner(busy=True)], jobs=jobs), JOBS)
        self.assertIn("hosted_load=5 hosted_cap=5", self.log)

    def test_hosted_cap_preserves_priority_after_idle_assignment(self):
        jobs = [hosted_job("xcode-27")] * 4
        outputs = self.pick(labels=SELF_HOSTED, runners=[runner()], jobs=jobs)
        self.assert_routes(outputs, ("macos_app", "server", "spec", "runner_hook"))

    def test_missing_status_token_falls_back_without_api(self):
        self.assert_routes(self.pick(labels=SELF_HOSTED, env={"RUNNER_STATUS_TOKEN": ""}))
        self.assertEqual(self.requests, [])

    def test_offline_and_nonmatching_runners_fall_back(self):
        runners = [runner(status="offline"), runner(labels=["self-hosted", "other-mac"])]
        self.assert_routes(self.pick(labels=SELF_HOSTED, runners=runners))
        self.assertEqual(self.requests, ["runners?per_page=100"])

    def test_runner_api_failure_falls_back(self):
        self.assert_routes(self.pick(labels=SELF_HOSTED, fixtures={"runners?per_page=100": None}))

    def test_malformed_runner_response_falls_back(self):
        self.assert_routes(self.pick(labels=SELF_HOSTED, fixtures={"runners?per_page=100": {}}))

    def test_hosted_api_failure_keeps_idle_then_falls_back(self):
        outputs = self.pick(labels=SELF_HOSTED, runners=[runner()], fixtures={
            "runs?status=queued&per_page=100&page=1": None,
        })
        self.assert_routes(outputs, ("macos_app",))
        self.assertIn("hosted_load=unknown", self.log)

    def test_malformed_hosted_api_response_falls_back(self):
        for path in ("runs?status=queued&per_page=100&page=1", "runs/1/jobs?filter=latest&per_page=100&page=1"):
            with self.subTest(path=path):
                self.assert_routes(self.pick(labels=SELF_HOSTED, runners=[runner(busy=True)], fixtures={path: {}}))
                self.assertIn("hosted_load=unknown", self.log)

    def test_invalid_runner_arrays_fail(self):
        for labels in ("invalid", [], [1], {"label": "macos-15"}):
            with self.subTest(labels=labels):
                result, outputs = self.run_script(inline_script("pick-macos"), {
                    "EVENT": "push", "RUNS_ON": json.dumps(labels),
                })
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("MACOS_RUNS_ON variable must be a JSON array", result.stdout)
                self.assertEqual(outputs, {})

    def test_workflow_changes_run_server_and_spec_but_ordinary_paths_skip(self):
        self.write_executable("git", """
            import os
            import sys
            if sys.argv[1:] == ['rev-parse', 'HEAD^1']:
                print('fixture-base')
            elif sys.argv[1:] == ['diff', '-z', '--name-only', '--no-renames', 'fixture-base', 'HEAD']:
                sys.stdout.write(os.environ['CHANGED'] + '\\0')
            else:
                sys.exit('Unexpected git call')
        """)
        scripts = self.dir / "scripts"
        scripts.mkdir()
        for name in ("test-macos-changes.sh", "macos-changes.sh"):
            path = scripts / name
            path.write_text("#!/bin/bash\nexit 0\n")
            path.chmod(0o755)
        for changed in (".github/workflows/ci.yml", "server/src/index.ts", "spec/proto/api.proto", "site/index.html"):
            with self.subTest(changed=changed):
                result, outputs = self.run_script(inline_script("changes"), {"EVENT": "pull_request", "CHANGED": changed})
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                wanted = "true" if changed == ".github/workflows/ci.yml" else "false"
                self.assertEqual(outputs["server_macos"], wanted)
                self.assertEqual(outputs["spec_macos"], wanted)
                self.assertEqual(outputs["site_only"], "true" if changed.startswith("site/") else "false")


if __name__ == "__main__":
    unittest.main()
