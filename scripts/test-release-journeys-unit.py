#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import copy
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shlex
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("release_journeys", Path(__file__).with_name("test-release-journeys.py"))
DRIVER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DRIVER)


class ReleaseJourneys(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="release-unit-", dir=DRIVER.REPO)
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def evidence(self, scenario="verify"):
        return DRIVER.Evidence(self.root / "evidence", "v0.1.0-alpha.3", scenario)

    def passing_report(self, scenario):
        return {"scenario": scenario, "assertions": [{"name": name, "passed": True}
                for name in DRIVER.REQUIRED[scenario]], "cleanup": [{"name": "owned state", "passed": True}], "errors": []}

    def test_strict_tags(self):
        for tag in ("v0.1.0-alpha.3", "v1.2.3", "v10.20.30-rc.0"):
            with self.subTest(tag=tag):
                self.assertEqual(DRIVER.tag_value(tag), tag)
        for tag in ("0.1.0", "v01.2.3", "v1.2.3-alpha.01", "v1.2.3-beta.1", "v1.2.3\n",
                    "v1.2.3/../../main", "v1.2.3;touch nope", "v1.2.3+meta", "v1.2.3-rc.-1"):
            with self.subTest(tag=tag), self.assertRaises(argparse.ArgumentTypeError):
                DRIVER.tag_value(tag)

    def test_checksums_allow_standard_text_and_binary_markers(self):
        result = DRIVER.checksums("a" * 64 + "  koi.tar.gz\n" + "b" * 64 + " *Koidrive.dmg\n")
        self.assertEqual(result, {"koi.tar.gz": "a" * 64, "Koidrive.dmg": "b" * 64})

    def test_checksums_reject_duplicates_and_ambiguous_names(self):
        valid = "a" * 64 + "  koi.tar.gz\n"
        for text in ("", valid + valid, valid + "b" * 64 + "  koi.tar.gz\n", "a" * 63 + "  koi.tar.gz",
                     "A" * 64 + "  koi.tar.gz", "a" * 64 + "  ../koi", "a" * 64 + "  /koi",
                     "a" * 64 + "  koi extra", valid + "\n", "a" * 64 + "\tkoi.tar.gz"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                DRIVER.checksums(text)

    def test_checksum_mismatch_cannot_reach_execution(self):
        evidence = self.evidence()

        def fake_download(_, url, path):
            path.write_text("a" * 64 + "  koidrive-aarch64-apple-darwin.tar.gz\n" if url.endswith("SHA256SUMS") else "wrong bytes")

        with patch.object(DRIVER, "download", side_effect=fake_download), patch.object(evidence, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "checksum"):
                DRIVER.release_assets(evidence, self.root)
            run.assert_not_called()
        self.assertFalse(DRIVER.passed(evidence.report))

    def test_each_hosted_guard_condition_is_required(self):
        valid = {"env": {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"},
                 "system": "Darwin", "machine": "arm64", "home": self.root, "account_home": self.root}
        DRIVER.hosted_guard(**valid)
        for change in ({"env": {}}, {"env": {"GITHUB_ACTIONS": "true"}},
                       {"env": {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "self-hosted"}},
                       {"system": "Linux"}, {"machine": "x86_64"}, {"home": self.root / "fake-home"}):
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                DRIVER.hosted_guard(**{**valid, **change})

    def test_hosted_guard_rejects_profile_and_release_overrides(self):
        for key in ("XDG_STATE_HOME", "XDG_CONFIG_HOME", "KOIDRIVE_RELEASE_BASE_URL", "KOIDRIVE_JOURNEY_PROBE", "KOIDRIVE_HELD_INSTALLER_FILE",
                    "HOMEBREW_CASK_OPTS", "HOMEBREW_NO_CASK_QUARANTINE", "ZDOTDIR"):
            with self.subTest(key=key), self.assertRaisesRegex(RuntimeError, "overrides"):
                DRIVER.hosted_guard(env={"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", key: "fake"},
                                    system="Darwin", machine="arm64", home=self.root, account_home=self.root)

    def test_main_refuses_local_runtime_before_starting_any_command(self):
        destination = self.root / "evidence"
        with patch.dict(os.environ, {}, clear=True), patch.object(DRIVER.subprocess, "Popen") as popen, patch("builtins.print"):
            status = DRIVER.main(["--tag", "v0.1.0-alpha.3", "--scenario", "installer", "--evidence", str(destination)])
        popen.assert_not_called()
        report = json.loads((destination / "report.json").read_text())
        self.assertEqual(status, 1)
        self.assertFalse(report["passed"])
        self.assertIn("GitHub-hosted", report["errors"][0])

    def test_evidence_directory_must_be_new(self):
        self.evidence()
        with self.assertRaises(FileExistsError):
            self.evidence()

    def test_empty_report_cannot_pass(self):
        for scenario in DRIVER.SCENARIOS:
            with self.subTest(scenario=scenario):
                report = {"scenario": scenario, "assertions": [], "cleanup": [], "errors": []}
                self.assertFalse(DRIVER.passed(report))

    def test_every_required_assertion_and_cleanup_is_required(self):
        for scenario in DRIVER.SCENARIOS:
            report = self.passing_report(scenario)
            self.assertTrue(DRIVER.passed(report))
            for index in range(len(report["assertions"])):
                changed = copy.deepcopy(report)
                changed["assertions"].pop(index)
                self.assertFalse(DRIVER.passed(changed))
                changed = copy.deepcopy(report)
                changed["assertions"][index]["passed"] = False
                self.assertFalse(DRIVER.passed(changed))
            for change in ({"errors": ["failed"]}, {"cleanup": []}, {"cleanup": [{"passed": False}]}):
                self.assertFalse(DRIVER.passed({**report, **change}))

    def test_later_passing_assertion_cannot_hide_earlier_failure(self):
        report = self.passing_report("installer")
        report["assertions"].insert(0, {"name": "attended setup", "passed": False})
        self.assertFalse(DRIVER.passed(report))

    def test_assertion_requires_true_not_truthy_serialized_status(self):
        report = self.passing_report("cask")
        report["assertions"][0]["passed"] = "SKIP"
        self.assertFalse(DRIVER.passed(report))

    def test_cleanup_failure_is_recorded_and_later_cleanup_runs(self):
        evidence = self.evidence()
        self.assertFalse(evidence.cleanup("failing", lambda: (_ for _ in ()).throw(RuntimeError("failed"))))
        self.assertTrue(evidence.cleanup("later", lambda: None))
        self.assertEqual([item["passed"] for item in evidence.report["cleanup"]], [False, True])

    def test_cleanup_refuses_existing_or_unclaimed_state(self):
        ownership = DRIVER.Ownership()
        foreign = self.root / "foreign"
        foreign.write_text("keep")
        with self.assertRaises(RuntimeError):
            ownership.claim(foreign)
        with self.assertRaises(RuntimeError):
            ownership.remove(foreign)
        self.assertEqual(foreign.read_text(), "keep")

    @unittest.skipUnless(os.name == "posix", "Symlink ownership probe requires POSIX")
    def test_cleanup_removes_owned_symlink_not_its_target(self):
        ownership = DRIVER.Ownership()
        foreign = self.root / "foreign"
        foreign.mkdir()
        (foreign / "document").write_text("keep")
        owned = ownership.claim(self.root / "owned")
        owned.symlink_to(foreign, target_is_directory=True)
        ownership.remove(owned)
        self.assertFalse(owned.is_symlink())
        self.assertEqual((foreign / "document").read_text(), "keep")

    @unittest.skipUnless(os.name == "posix", "Symlink ownership probe requires POSIX")
    def test_cleanup_refuses_changed_parent_symlink(self):
        ownership = DRIVER.Ownership()
        parent = self.root / "parent"
        parent.mkdir()
        owned = ownership.claim(parent / "owned")
        parent.rmdir()
        foreign = self.root / "foreign"
        foreign.mkdir()
        (foreign / "owned").write_text("keep")
        parent.symlink_to(foreign, target_is_directory=True)
        with self.assertRaises(RuntimeError):
            ownership.remove(owned)
        self.assertEqual((foreign / "owned").read_text(), "keep")

    @unittest.skipUnless(os.name == "posix", "Symlink ownership probe requires POSIX")
    def test_cleanup_refuses_claim_through_symlinked_parent(self):
        ownership = DRIVER.Ownership()
        target = self.root / "target"
        target.mkdir()
        parent = self.root / "parent"
        parent.symlink_to(target, target_is_directory=True)
        with self.assertRaises(RuntimeError):
            ownership.claim(parent / "new-state")

    def test_tap_waits_for_exact_version_and_checks_real_url_and_digest(self):
        evidence = self.evidence("cask")
        digest = "a" * 64
        name = "Koidrive-0.1.0-alpha.3-arm64.dmg"
        versions = iter(["0.1.0-alpha.2", "0.1.0-alpha.3", "0.1.0-alpha.3"])

        def brew(argv, **_):
            if argv == ["brew", "--version"]:
                return 0, "Homebrew 7.0.0\n", ""
            if argv == ["brew", "tap"]:
                return 0, "koidrive/tap\n", ""
            if argv[:2] == ["brew", "info"]:
                data = {"casks": [{"version": next(versions), "sha256": digest,
                        "url": DRIVER.RELEASE + "/v0.1.0-alpha.3/" + name, "depends_on": {}}]}
                return 0, json.dumps(data), ""
            return 0, "real tap source", ""

        with patch.object(evidence, "run", side_effect=brew) as run, patch.object(DRIVER.time, "sleep"):
            DRIVER.brew_prepare(evidence, {name: digest}, "cask")
        self.assertIn(unittest.mock.call(["brew", "update", "--quiet"], timeout=120), run.call_args_list)
        self.assertIn(unittest.mock.call(["brew", "trust", "--cask", DRIVER.TAP]), run.call_args_list)

    def test_tap_wrong_version_does_not_fall_back_to_a_fixture(self):
        evidence = self.evidence("cask")

        def brew(argv, **_):
            if argv[:2] == ["brew", "info"]:
                return 0, json.dumps({"casks": [{"version": "9.9.9"}]}), ""
            return 0, "Homebrew 6.0.0\n" if argv[1] == "--version" else "koidrive/tap\n", ""

        with patch.object(evidence, "run", side_effect=brew) as run:
            with self.assertRaisesRegex(RuntimeError, "publication wait expired"):
                DRIVER.brew_prepare(evidence, {}, "cask", wait_seconds=0)
        self.assertTrue(all(call.args[0][:2] != ["brew", "install"] for call in run.call_args_list))

    def test_formula_trust_is_granted_for_formula_not_cask(self):
        evidence = self.evidence("formula-conflict")
        digest = "a" * 64
        name = "koidrive-aarch64-apple-darwin.tar.gz"
        data = {"formulae": [{"versions": {"stable": "0.1.0-alpha.3"}, "urls": {"stable": {
            "checksum": digest, "url": DRIVER.RELEASE + "/v0.1.0-alpha.3/" + name}}}]}

        def brew(argv, **_):
            if argv[:2] == ["brew", "info"]:
                return 0, json.dumps(data), ""
            return 0, "Homebrew 7.0.0\n" if argv[1] == "--version" else "koidrive/tap\n", ""

        with patch.object(evidence, "run", side_effect=brew) as run:
            DRIVER.brew_prepare(evidence, {name: digest}, "formula")
        self.assertIn(unittest.mock.call(["brew", "trust", "--formula", DRIVER.TAP]), run.call_args_list)
        self.assertNotIn(unittest.mock.call(["brew", "trust", "--cask", DRIVER.TAP]), run.call_args_list)

    def test_conflict_summary_starts_with_exact_collision_line(self):
        evidence = self.evidence("formula-conflict")
        collision = "Error: It seems there is already a Binary at '/opt/homebrew/bin/koi'."
        evidence.report["conflict"] = collision
        with patch("builtins.print"):
            evidence.close()
        self.assertTrue((evidence.directory / "summary.txt").read_text().startswith(collision + "\n"))

    def test_quarantine_preservation_allows_os_flag_and_provenance_changes(self):
        before = "/Applications/Koidrive.app: com.apple.quarantine: 0083;initial\n"
        after = ("/Applications/Koidrive.app: com.apple.quarantine: 00c3;approved\n"
                 "/Applications/Koidrive.app: com.apple.provenance: changed\n")
        self.assertLessEqual(DRIVER.quarantine_paths(before), DRIVER.quarantine_paths(after))
        self.assertFalse(DRIVER.quarantine_paths(before) <= DRIVER.quarantine_paths("com.apple.provenance: changed\n"))
        self.assertEqual(DRIVER.quarantine_paths(""), set())
        self.assertEqual(DRIVER.quarantine_paths("com.apple.quarantine: 0083;initial\n"), {"."})

    def test_redaction_handles_unknown_claim_pairing_and_json_credentials(self):
        for text, secret in (("Koidrive claim code (valid for 1 hour): ABCD12345678\n", "ABCD12345678"),
                             ('"token": "secret-token"\n', "secret-token"),
                             ('"recovery_key": "secret-key"\n', "secret-key"),
                             ('  "K7QM-2XPD",\n', "K7QM-2XPD"),
                             ('  "K7QM01PD",\n', "K7QM01PD"),
                             ("Authorization: Bearer secret-token\n", "secret-token"),
                             ('"join_argv": ["koi", "join", "short-secret"]', "short-secret")):
            with self.subTest(text=text):
                self.assertNotIn(secret, DRIVER.sanitized(text))
        digest = "a" * 64
        self.assertIn(digest, DRIVER.sanitized("SHA256 " + digest))
        self.assertEqual(DRIVER.sanitized("SHA256SUMS"), "SHA256SUMS")

    def test_only_the_expected_binary_collision_counts_as_conflict(self):
        binary = self.root / "bin/koi"
        collision = f"Error: It seems there is already a Binary at '{binary}'.\n"
        self.assertTrue(DRIVER.binary_conflict(1, collision, binary))
        for status, output in ((0, collision), (124, collision), (-9, collision),
                               (1, "Error: network conflict installing koi"),
                               (1, "Error: koidrive/tap is not trusted"),
                               (1, "Error: It seems there is already a Binary at '/foreign/koi'."),
                               (1, f"Already installed formula {binary}; network error")):
            with self.subTest(status=status, output=output):
                self.assertFalse(DRIVER.binary_conflict(status, output, binary))

    def test_capture_redacts_before_persisting_claim_stream(self):
        evidence = self.evidence()
        text = "Koidrive claim code (valid for 1 hour): ABCD12345678\n"
        _, raw, _ = evidence.run([sys.executable, "-c", "print(" + repr(text) + ",end='')"])
        self.assertIn("ABCD12345678", raw)
        persisted = (evidence.directory / evidence.report["commands"][0]["stdout"]).read_text()
        self.assertNotIn("ABCD12345678", persisted)
        self.assertIn("[REDACTED]", persisted)

    def test_native_cleanup_probes_only_owned_labels_and_rejects_unknown_or_loaded_state(self):
        evidence = self.evidence("installer")
        label = "dev.koidrive.koi.abcdef12"
        expected = ["/bin/launchctl", "print", f"gui/{os.getuid()}/{label}"]
        with patch.object(evidence, "run", return_value=(113, "", "Could not find service")) as run:
            DRIVER.assert_native_absent(evidence, [label])
        run.assert_called_once_with(expected, required=False)
        for status, output, error in ((0, "state = running", ""), (124, "", ""), (1, "", "permission denied")):
            with self.subTest(status=status, error=error), patch.object(evidence, "run", return_value=(status, output, error)):
                with self.assertRaisesRegex(RuntimeError, "Could not prove owned service unloaded"):
                    DRIVER.assert_native_absent(evidence, [label])

    def test_clean_account_snapshot_queries_launchd_without_a_global_btm_probe(self):
        evidence = self.evidence("cask")
        with patch.object(evidence, "run", return_value=(0, "GUI domain", "")) as run:
            self.assertEqual(DRIVER.launchd_state(evidence), "GUI domain")
        run.assert_called_once_with(["/bin/launchctl", "print", f"gui/{os.getuid()}"])

    def test_cask_requires_quarantine_on_the_bundle_not_only_a_nested_file(self):
        evidence = self.evidence("cask")
        app = self.root / "Koidrive.app"
        nested = str(app / "Contents/Helpers/koi") + ": com.apple.quarantine: 0081;test\n"
        with patch.object(evidence, "run", side_effect=[(0, nested, ""), (1, "", "No such xattr")]):
            with self.assertRaisesRegex(RuntimeError, "app quarantine"):
                DRIVER.quarantine(evidence, app)

    def test_failed_command_is_not_silently_accepted(self):
        evidence = self.evidence()
        with self.assertRaises(RuntimeError):
            evidence.run([sys.executable, "-c", "raise SystemExit(7)"])
        self.assertEqual(evidence.report["commands"][0]["status"], 7)
        status, _, _ = evidence.run([sys.executable, "-c", "raise SystemExit(7)"], required=False)
        self.assertEqual(status, 7)

    def test_version_rejects_wrong_extra_or_missing_output(self):
        evidence = self.evidence()
        for status, output in ((1, "koi 0.1.0-alpha.3\n"), (0, "koi 0.1.0-alpha.2\n"),
                               (0, "warning\nkoi 0.1.0-alpha.3\n"), (0, "koi 0.1.0-alpha.3\n\n"), (0, "")):
            with self.subTest(status=status, output=output), patch.object(evidence, "run", return_value=(status, output, "")):
                with self.assertRaises(RuntimeError):
                    DRIVER.version(evidence, "never-executed-koi", "exact version")

    @unittest.skipUnless(os.name == "posix", "Controlling PTYs require POSIX")
    def test_pty_keeps_installer_stdin_a_pipe_and_answers_real_tty(self):
        evidence = self.evidence("installer")
        folder = self.root / "Sync"
        probe = self.root / "harmless-probe.sh"
        python = "\n".join([
            "import os,stat,sys",
            "assert stat.S_ISFIFO(os.fstat(0).st_mode)",
            "assert not os.isatty(0) and os.isatty(1) and os.isatty(2)",
            "assert os.environ.get('CI') is None and os.environ['TERM']=='dumb'",
            "assert os.environ['DO_NOT_TRACK']=='1'",
            "assert os.environ['PATH']==" + repr(DRIVER.SYSTEM_PATH),
            "tty=open('/dev/tty','r')",
            "assert os.tcgetpgrp(tty.fileno())==os.getpgrp()",
            "print('Which shared folder should sync here?\\n  1. Journey\\n  2. Create a shared folder\\nChoose 1 to 2: ',end='',flush=True)",
            "assert tty.readline().strip()=='1'",
            "print('Press Enter to use this folder, or type another path: ',end='',flush=True)",
            "assert tty.readline().strip()==" + repr(str(folder)),
            "print(" + repr(f"Create {folder}? [Y/n] ") + ",end='',flush=True)",
            "assert tty.readline().strip()=='y'",
            "print(" + repr(f"Done. Linked {folder} to Journey.") + ")",
        ])
        probe.write_text("exec " + shlex.quote(sys.executable) + " -c " + shlex.quote(python) + "\n")
        code, transcript, answers = DRIVER.attended(evidence, DRIVER.pipeline(probe), folder, timeout=10)
        self.assertEqual(code, 0, transcript)
        self.assertEqual(answers, 3)
        self.assertIn(f"Done. Linked {folder} to Journey.", transcript)

    @unittest.skipUnless(os.name == "posix", "Controlling PTYs require POSIX")
    def test_pty_without_prompts_does_not_synthesize_completion(self):
        evidence = self.evidence("installer")
        code, transcript, answers = DRIVER.attended(evidence, "printf 'no setup happened\\n'", self.root / "Sync", timeout=5)
        self.assertEqual(code, 0)
        self.assertEqual(answers, 0)
        self.assertNotIn("Done. Linked", transcript)

    def test_pipeline_quotes_file_url_without_options_or_setup_bypass(self):
        command = DRIVER.pipeline(self.root / "a 'b; c.sh")
        self.assertTrue(command.startswith("curl -fsSL -- file://"))
        self.assertTrue(command.endswith(" | sh"))
        self.assertNotIn("--no-app", command)
        self.assertNotIn("--no-quarantine", command)

    def test_stop_group_refuses_foreign_process_group(self):
        process = unittest.mock.Mock(pid=12345)
        process.poll.return_value = None
        with patch.object(DRIVER.os, "getpgid", return_value=999, create=True), patch.object(DRIVER.os, "killpg", create=True) as killpg:
            with self.assertRaises(RuntimeError):
                DRIVER.stop_group(process)
        killpg.assert_not_called()

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_native_service_cleanup_refuses_foreign_profile_or_binary(self, _uid):
        evidence = self.evidence("installer")
        agents = self.root / "Library/LaunchAgents"
        agents.mkdir(parents=True)
        path = agents / "dev.koidrive.koi.plist"
        binary = self.root / ".local/bin/koi"
        own_state = str(self.root / ".local/state")
        spec = {"Label": "dev.koidrive.koi", "ProgramArguments": [str(binary), "_daemon"],
                "EnvironmentVariables": {"XDG_STATE_HOME": own_state}}
        for change in ({"ProgramArguments": ["/foreign/koi", "_daemon"]},
                       {"EnvironmentVariables": {"XDG_STATE_HOME": "/foreign/state"}},
                       {"Label": "dev.koidrive.koi.foreign"}):
            path.write_bytes(plistlib.dumps({**spec, **change}))
            with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "foreign launch agent"):
                    DRIVER.service_cleanup(evidence, {own_state}, [binary])
            run.assert_not_called()
            self.assertTrue(path.exists())

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_native_service_cleanup_only_removes_after_confirmed_bootout(self, _uid):
        evidence = self.evidence("installer")
        agents = self.root / "Library/LaunchAgents"
        agents.mkdir(parents=True)
        path = agents / "dev.koidrive.koi.plist"
        binary = self.root / ".local/bin/koi"
        own_state = str(self.root / ".local/state")
        spec = {"Label": "dev.koidrive.koi", "ProgramArguments": [str(binary), "_daemon"],
                "EnvironmentVariables": {"XDG_STATE_HOME": own_state}}
        path.write_bytes(plistlib.dumps(spec))
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", return_value=(0, "", "")):
            with self.assertRaisesRegex(RuntimeError, "prove owned service unloaded"):
                DRIVER.service_cleanup(evidence, {own_state}, [binary])
        self.assertTrue(path.exists())
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", side_effect=[(0, "", ""), (1, "", "Could not find service")]):
            labels = DRIVER.service_cleanup(evidence, {own_state}, [binary])
        self.assertFalse(path.exists())
        self.assertEqual(labels, ["dev.koidrive.koi"])

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_peer_service_cleanup_uses_its_produced_label(self, _uid):
        evidence = self.evidence("installer")
        agents = self.root / "Library/LaunchAgents"
        agents.mkdir(parents=True)
        label = "dev.koidrive.koi.1234abcd"
        path = agents / (label + ".plist")
        binary = self.root / ".local/bin/koi"
        state = str(self.root / "peer/state")
        spec = {"Label": label, "ProgramArguments": [str(binary), "_daemon"],
                "EnvironmentVariables": {"XDG_STATE_HOME": state}}
        path.write_bytes(plistlib.dumps(spec))
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", side_effect=[(0, "", ""), (1, "", "Could not find service")]) as run:
            labels = DRIVER.service_cleanup(evidence, {state}, [binary])
        self.assertEqual(labels, [label])
        self.assertIn(label, run.call_args_list[0].args[0][-1])
        self.assertFalse(path.exists())

    def test_launchctl_permission_or_timeout_failure_is_not_proof_of_cleanup(self):
        self.assertTrue(DRIVER.unloaded(113, "Could not find service"))
        for status, error in ((0, "Could not find service"), (124, "Could not find service"),
                              (-9, "Could not find service"), (1, "Operation not permitted"), (1, "")):
            self.assertFalse(DRIVER.unloaded(status, error))


if __name__ == "__main__":
    unittest.main(verbosity=2)
