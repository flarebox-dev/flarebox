#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import errno
import hashlib
import os
import platform
from pathlib import Path
import pty
import select
import shlex
import shutil
import signal
import subprocess
import tarfile
import tempfile
import textwrap
import time
import unittest

ROOT = Path(__file__).resolve().parent.parent


class InstallJourney(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="koidrive-install-journey-")
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.home = self.dir / "home"
        self.home.mkdir()
        self.bin = self.dir / "tools"
        self.bin.mkdir()
        self.env = {
            "HOME": str(self.home),
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "SHELL": "/bin/zsh",
            "KOIDRIVE_VERSION": "0.0.0",
            "KOIDRIVE_RELEASE_BASE_URL": (self.dir / "releases").as_uri(),
            "TERM": "xterm",
        }
        self.tool("uname", 'case "$1" in -s) echo "${TEST_OS:-Darwin}";; -m) echo arm64;; esac')
        for tool in ("launchctl", "systemctl", "ps"):
            self.tool(tool, f'printf "%s\\n" "{tool} $*" >> "$HOME/services.log"\nexit 1')
        self.fixture = self.dir / "koi"
        self.fixture.write_text(textwrap.dedent("""\
            #!/bin/sh
            set -eu
            if [ "${1:-}" = --version ]; then
                echo 'koi 0.0.0 Koidrive sync client'
                exit
            fi
            printf '%s\\n' "$0" "$@" > "$HOME/setup.args"
            [ -t 0 ] && [ -t 2 ] || exit 91
            printf 'SETUP_READY\\n'
            IFS= read -r answer
            printf '%s\\n' "$answer" > "$HOME/setup.answer"
            exit "${TEST_SETUP_EXIT:-0}"
        """))
        self.fixture.chmod(0o755)
        release = self.dir / "releases/download/v0.0.0"
        release.mkdir(parents=True)
        sums = []
        for platform in ("aarch64-apple-darwin", "aarch64-unknown-linux-musl"):
            archive = release / f"koidrive-{platform}.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                tar.add(self.fixture, arcname="koi")
            sums.append(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
        self.sums = release / "SHA256SUMS"
        self.sums.write_text("".join(sums))

    def tool(self, name, body):
        file = self.bin / name
        file.write_text("#!/bin/sh\n" + body + "\n")
        file.chmod(0o755)

    def install(self, *args, terminal=True, piped=True, env=None, redirect=None):
        environment = self.env | (env or {})
        installer = shlex.quote(str(ROOT / "install.sh"))
        options = " ".join(shlex.quote(arg) for arg in args)
        command = f"cat {installer} | /bin/sh -s -- {options}" if piped else f"/bin/sh {installer} {options}"
        if redirect:
            command += f' {"2>" if redirect == "stderr" else ">"} "$HOME/redirect.log"'
        if not terminal:
            result = subprocess.run(
                ["/bin/sh", "-c", command], env=environment, input="",
                capture_output=True, text=True, timeout=15,
            )
            return result.returncode, result.stdout + result.stderr
        pid, fd = pty.fork()
        if pid == 0:
            os.execve("/bin/sh", ["/bin/sh", "-c", command], environment)
        output = bytearray()
        answered = False
        deadline = time.monotonic() + 15
        try:
            while time.monotonic() < deadline:
                if not select.select([fd], [], [], 0.1)[0]:
                    continue
                try:
                    chunk = os.read(fd, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                output.extend(chunk)
                if not answered and b"SETUP_READY" in output:
                    os.write(fd, b"continue\n")
                    answered = True
            else:
                self.fail("installer did not finish before the terminal timeout")
            _, status = os.waitpid(pid, 0)
            pid = None
            return os.waitstatus_to_exitcode(status), output.decode().replace("\r\n", "\n")
        finally:
            os.close(fd)
            if pid is not None:
                os.killpg(pid, signal.SIGKILL)
                os.waitpid(pid, 0)

    def successful_install(self, *args, **kwargs):
        code, output = self.install(*args, **kwargs)
        self.assertEqual(code, 0, output)
        self.assertEqual((self.home / ".local/bin/koi").read_bytes(), self.fixture.read_bytes())
        return output

    def test_piped_mac_installer_enters_setup_with_terminal_stdin(self):
        output = self.successful_install()
        self.assertEqual((self.home / "setup.args").read_text().splitlines(), [str(self.home / ".local/bin/koi"), "setup"])
        self.assertEqual((self.home / "setup.answer").read_text(), "continue\n")
        profile = self.home / ".zprofile"
        self.assertIn(f"Updated {profile}:\n+ {profile.read_text()}", output)
        self.assertIn("SHA256 verified", output)

    def test_no_app_is_forwarded_to_setup(self):
        self.successful_install("--no-app")
        self.assertEqual((self.home / "setup.args").read_text().splitlines()[-2:], ["setup", "--no-app"])

    def test_setup_failure_is_not_reported_as_success(self):
        code, _ = self.install(env={"TEST_SETUP_EXIT": "23"})
        self.assertEqual(code, 23)
        self.assertTrue((self.home / ".local/bin/koi").is_file())

    def test_profiles_are_idempotent_and_preserve_existing_text(self):
        for shell, profile in (("zsh", ".zprofile"), ("bash", ".bash_profile"), ("fish", ".config/fish/conf.d/koidrive.fish")):
            with self.subTest(shell=shell):
                file = self.home / profile
                file.parent.mkdir(parents=True, exist_ok=True)
                file.write_text("export EXISTING=kept\n" if shell != "fish" else "set -gx EXISTING kept\n")
                file.chmod(0o600)
                self.successful_install("--no-setup", env={"SHELL": f"/bin/{shell}"})
                first = file.read_text()
                output = self.successful_install("--no-setup", env={"SHELL": f"/bin/{shell}"})
                self.assertEqual(file.read_text(), first)
                self.assertEqual(first.count("koidrive PATH"), 1)
                self.assertIn("EXISTING", first)
                self.assertEqual(file.stat().st_mode & 0o777, 0o600)
                self.assertNotIn(f"Updated {file}", output)

    def test_profiles_make_koi_available_without_duplicate_entries(self):
        for shell, profile in (("zsh", ".zprofile"), ("bash", ".bash_profile")):
            executable = shutil.which(shell)
            self.assertIsNotNone(executable, f"missing shell: {shell}")
            with self.subTest(shell=shell):
                self.successful_install("--no-setup", env={"SHELL": f"/bin/{shell}"})
                file = shlex.quote(str(self.home / profile))
                command = f"source {file}; source {file}; command -v koi"
                result = subprocess.run([executable, "-c", command], env=self.env, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), str(self.home / ".local/bin/koi"))
                command = f"source {file}; source {file}; printf '%s\\n' \"$PATH\""
                result = subprocess.run([executable, "-c", command], env=self.env, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.count(str(self.home / ".local/bin")), 1)

    def test_profile_short_write_preserves_contents_and_permissions(self):
        for linked in (False, True):
            with self.subTest(linked=linked):
                profile = self.home / ".zprofile"
                profile.unlink(missing_ok=True)
                target = self.home / "saved-profile" if linked else profile
                target.write_text("export EXISTING=kept\nexport SECOND=also_kept\n")
                target.chmod(0o640)
                before = target.read_bytes()
                if linked:
                    profile.symlink_to(target.name)
                self.tool("cat", '''case "$1" in */profile) head -c 7 "$1"; exit 74;; esac
exec /bin/cat "$@"''')
                output = self.successful_install("--no-setup")
                self.assertEqual(target.read_bytes(), before)
                self.assertEqual(target.stat().st_mode & 0o777, 0o640)
                self.assertEqual(profile.is_symlink(), linked)
                if linked:
                    self.assertEqual(os.readlink(profile), target.name)
                self.assertIn("could not update PATH", output)
                self.assertNotIn("Updated ", output)
                self.assertEqual(list(self.home.rglob(".koidrive*")), [])

    def test_profile_rename_failure_preserves_contents_and_permissions(self):
        for linked in (False, True):
            with self.subTest(linked=linked):
                profile = self.home / ".zprofile"
                profile.unlink(missing_ok=True)
                target = self.home / "saved-profile" if linked else profile
                target.write_text("export EXISTING=kept\n")
                target.chmod(0o640)
                before = target.read_bytes()
                if linked:
                    profile.symlink_to(target.name)
                self.tool("mv", '''case "$3" in */.zprofile|*/saved-profile) exit 75;; esac
exec /bin/mv "$@"''')
                output = self.successful_install("--no-setup")
                self.assertEqual(target.read_bytes(), before)
                self.assertEqual(target.stat().st_mode & 0o777, 0o640)
                self.assertEqual(profile.is_symlink(), linked)
                self.assertIn("could not update PATH", output)
                self.assertNotIn("Updated ", output)
                self.assertEqual(list(self.home.rglob(".koidrive*")), [])

    def test_profile_symlink_chain_updates_target_and_preserves_links_and_mode(self):
        target = self.home / "dotfiles/login-profile"
        target.parent.mkdir()
        target.write_text("export EXISTING=kept\n")
        target.chmod(0o640)
        middle = self.home / "profile-link"
        middle.symlink_to(target)
        profile = self.home / ".zprofile"
        profile.symlink_to(middle.name)
        output = self.successful_install("--no-setup")
        self.assertTrue(profile.is_symlink())
        self.assertTrue(middle.is_symlink())
        self.assertEqual(os.readlink(profile), middle.name)
        self.assertEqual(os.readlink(middle), str(target))
        self.assertTrue(target.read_text().startswith("export EXISTING=kept\n"))
        self.assertEqual(target.read_text().count("koidrive PATH"), 1)
        self.assertEqual(target.stat().st_mode & 0o777, 0o640)
        self.assertIn(f"Updated {target}:\n", output)
        self.assertEqual(list(self.home.rglob(".koidrive*")), [])

    @unittest.skipUnless(shutil.which("fish"), "fish is not available")
    def test_fish_profile_quotes_custom_paths_and_is_idempotent(self):
        install_dir = self.home / "custom 'quoted' $bin `test` \\bin"
        code, output = self.install("--no-setup", env={"SHELL": "/bin/fish", "KOIDRIVE_INSTALL_DIR": str(install_dir)})
        self.assertEqual(code, 0, output)
        file = shlex.quote(str(self.home / ".config/fish/conf.d/koidrive.fish"))
        result = subprocess.run(
            [shutil.which("fish"), "--no-config", "-c", f"source {file}; source {file}; command -v koi; printf '%s\\n' $PATH"],
            env=self.env, capture_output=True, text=True, timeout=5,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], str(install_dir / "koi"))
        self.assertEqual(result.stdout.splitlines()[1:].count(str(install_dir)), 1)

    def test_path_opt_out_flags_and_environment_leave_profiles_alone(self):
        for options, env in ((("--no-modify-path",), {}), ((), {"KOIDRIVE_NO_MODIFY_PATH": "1"})):
            with self.subTest(options=options, env=env):
                output = self.successful_install(*options, env=env)
                self.assertFalse((self.home / ".zprofile").exists())
                self.assertTrue((self.home / "setup.answer").exists())
                self.assertIn("not on PATH", output)

    def test_setup_opt_out_flags_and_environment_still_configure_path(self):
        for options, env in ((("--no-setup",), {}), ((), {"KOIDRIVE_NO_SETUP": "1"})):
            with self.subTest(options=options, env=env):
                output = self.successful_install(*options, env=env)
                self.assertFalse((self.home / "setup.args").exists())
                self.assertTrue((self.home / ".zprofile").exists())
                self.assertIn("To set up Koidrive, run:", output)

    def test_unattended_and_ci_do_not_modify_profiles_or_run_setup(self):
        for terminal, env in ((False, {}), (True, {"CI": "true"}), (True, {"CI": ""}), (True, {"CI": "0"})):
            with self.subTest(terminal=terminal, env=env):
                self.successful_install(terminal=terminal, env=env)
                self.assertFalse((self.home / "setup.args").exists())
                self.assertFalse((self.home / ".zprofile").exists())

    def test_redirecting_either_output_stream_skips_setup_and_profiles(self):
        for stream in ("stdout", "stderr"):
            with self.subTest(stream=stream):
                self.successful_install(redirect=stream)
                self.assertFalse((self.home / "setup.args").exists())
                self.assertFalse((self.home / ".zprofile").exists())

    def test_terminal_output_without_a_controlling_terminal_is_unattended(self):
        master, slave = pty.openpty()
        try:
            result = subprocess.run(
                ["/bin/sh", str(ROOT / "install.sh")], stdin=subprocess.DEVNULL,
                stdout=slave, stderr=slave, start_new_session=True, env=self.env, timeout=15,
            )
        finally:
            os.close(slave)
            os.close(master)
        self.assertEqual(result.returncode, 0)
        self.assertFalse((self.home / "setup.args").exists())
        self.assertFalse((self.home / ".zprofile").exists())

    def test_linux_prints_setup_instructions_without_starting_setup(self):
        output = self.successful_install(env={"TEST_OS": "Linux"})
        self.assertFalse((self.home / "setup.args").exists())
        self.assertIn("To set up Koidrive, run:", output)

    def test_path_already_present_leaves_profiles_alone(self):
        self.successful_install(env={"PATH": f"{self.home}/.local/bin:{self.env['PATH']}"})
        self.assertFalse((self.home / ".zprofile").exists())

    def test_bash_login_preserves_existing_profile_precedence(self):
        for fallback in (".profile", ".bash_login"):
            with self.subTest(fallback=fallback):
                (self.home / ".bash_profile").unlink(missing_ok=True)
                (self.home / fallback).write_text(f"export EXISTING={fallback}\n")
                output = self.successful_install("--no-setup", env={"SHELL": "/bin/bash"})
                profile = (self.home / ".bash_profile").read_text()
                self.assertEqual(profile.count("koidrive PATH"), 1)
                for line in profile.splitlines():
                    self.assertIn(f"+ {line}\n", output)
                result = subprocess.run(
                    ["/bin/bash", "-lc", 'printf "%s\\n" "$EXISTING"; command -v koi'],
                    env=self.env, capture_output=True, text=True, timeout=5,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(), [fallback, str(self.home / ".local/bin/koi")])

    def test_zsh_login_respects_exported_zdotdir(self):
        zdotdir = self.home / "zsh-config"
        self.successful_install("--no-setup", env={"ZDOTDIR": str(zdotdir)})
        self.assertFalse((self.home / ".zprofile").exists())
        result = subprocess.run(
            [shutil.which("zsh"), "-lc", "command -v koi"],
            env=self.env | {"ZDOTDIR": str(zdotdir)}, capture_output=True, text=True, timeout=5,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.home / ".local/bin/koi"))

    def test_symlinked_install_directory_on_path_leaves_profiles_alone(self):
        installed = self.home / ".local/bin"
        installed.mkdir(parents=True)
        linked = self.dir / "linked"
        linked.symlink_to(installed)
        self.successful_install(env={"PATH": f"{linked}:{self.env['PATH']}"})
        self.assertFalse((self.home / ".zprofile").exists())

    def test_other_koi_on_path_is_never_used_for_setup(self):
        self.tool("koi", 'echo WRONG_PROGRAM; exit 92')
        output = self.successful_install()
        self.assertNotIn("WRONG_PROGRAM", output)
        self.assertIn("another program", output)
        self.assertEqual((self.home / "setup.args").read_text().splitlines()[0], str(self.home / ".local/bin/koi"))

    def test_custom_path_is_quoted_and_one_marked_entry_is_replaced(self):
        self.successful_install("--no-setup")
        previous = (self.home / ".zprofile").read_text().strip()
        install_dir = self.home / "custom 'quoted' $bin `test` \\bin"
        code, output = self.install("--no-setup", env={"KOIDRIVE_INSTALL_DIR": str(install_dir)})
        self.assertEqual(code, 0, output)
        profile = (self.home / ".zprofile").read_text()
        self.assertEqual(profile.count("koidrive PATH"), 1)
        self.assertIn(f"- {previous}\n+ {profile}", output)
        for shell in ("/bin/bash", "/bin/zsh"):
            result = subprocess.run([shell, "-c", 'source "$HOME/.zprofile"; command -v koi'], env=self.env, capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), str(install_dir / "koi"))

    def test_fish_uses_xdg_config_home(self):
        config = self.home / "custom-config"
        self.successful_install("--no-setup", env={"SHELL": "/usr/bin/fish", "XDG_CONFIG_HOME": str(config)})
        self.assertTrue((config / "fish/conf.d/koidrive.fish").exists())

    def test_unknown_shell_prints_instructions_and_still_runs_setup(self):
        output = self.successful_install(env={"SHELL": "/bin/unknown"})
        self.assertFalse((self.home / ".zprofile").exists())
        self.assertIn("not on PATH", output)
        self.assertTrue((self.home / "setup.answer").exists())

    def test_checksum_failure_preserves_cli_and_does_not_start_setup(self):
        installed = self.home / ".local/bin/koi"
        installed.parent.mkdir(parents=True)
        installed.write_text("old Koidrive sync client")
        self.sums.write_text(self.sums.read_text().replace(self.sums.read_text()[:64], "0" * 64))
        code, output = self.install()
        self.assertNotEqual(code, 0)
        self.assertIn("Checksum mismatch", output)
        self.assertEqual(installed.read_text(), "old Koidrive sync client")
        self.assertFalse((self.home / "setup.args").exists())
        self.assertFalse((self.home / ".zprofile").exists())

    def test_update_style_captured_child_never_starts_setup(self):
        self.successful_install(terminal=False, piped=False)
        self.assertFalse((self.home / "setup.args").exists())
        self.assertFalse((self.home / ".zprofile").exists())

    def test_help_and_unknown_option_do_not_install(self):
        code, output = self.install("--help")
        self.assertEqual(code, 0, output)
        self.assertIn("KOIDRIVE_NO_SETUP=1", output)
        self.assertFalse((self.home / ".local").exists())
        code, output = self.install("--not-an-option")
        self.assertNotEqual(code, 0)
        self.assertIn("Unknown option", output)
        self.assertFalse((self.home / ".local").exists())

    def test_existing_installer_regressions(self):
        arch = "aarch64" if platform.machine() in ("arm64", "aarch64") else "x86_64"
        os_target = "apple-darwin" if platform.system() == "Darwin" else "unknown-linux-musl"
        result = subprocess.run(
            ["/bin/sh", str(ROOT / "scripts/test-install.sh"), str(self.fixture), f"{arch}-{os_target}"],
            env=self.env | {"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Passed ", result.stdout)
        print(result.stdout.splitlines()[-1])


if __name__ == "__main__":
    unittest.main()
