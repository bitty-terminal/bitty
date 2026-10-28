"""Hermetic tests for ``scripts/generate-official-catalog.py`` (issue #2461).

The generator must read the registry checkout named on the command line even
when its caller exports repository-location ``GIT_*`` variables (Git hooks such
as ``pre-commit`` do). Each fixture is a throwaway repository, so no network
access and no ``bitty-plugins`` checkout are required.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = (
    Path(__file__).resolve().parents[2] / "scripts" / "generate-official-catalog.py"
)
GITLINK_PIN = "5dfc2e20607a4d7efdcf1be89195b3edc22b7ca0"
MANIFEST_HASH = "sha256:" + "a" * 64
GIT_IDENTITY = (
    "-c",
    "user.name=Catalog Fixture",
    "-c",
    "user.email=catalog@example.invalid",
    "-c",
    "commit.gpgsign=false",
)


def run_git(root: Path, *args: str) -> str:
    """Run one git command with host config and hooks isolated."""
    env = dict(os.environ)
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_SYSTEM"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    result = subprocess.run(
        ["git", *GIT_IDENTITY, *args],
        cwd=root,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def commit_all(root: Path, message: str) -> str:
    run_git(root, "add", "-A")
    run_git(root, "commit", "-q", "-m", message)
    return run_git(root, "rev-parse", "HEAD")


def init_repo(root: Path) -> None:
    root.mkdir(parents=True)
    run_git(root, "init", "-q", "-b", "main")


def build_foreign_repo(root: Path) -> str:
    """A repository holding a different HEAD, used as the hostile GIT_DIR."""
    init_repo(root)
    (root / "README.md").write_text("foreign repository\n", encoding="utf-8")
    return commit_all(root, "foreign fixture")


def build_registry(root: Path) -> str:
    """A minimal official registry: one entry plus its plugins/bar gitlink."""
    init_repo(root)
    (root / ".gitmodules").write_text(
        '[submodule "plugins/bar"]\n'
        "\tpath = plugins/bar\n"
        "\turl = https://github.com/bitty-terminal/bar.git\n",
        encoding="utf-8",
    )
    official = root / "registry" / "official"
    official.mkdir(parents=True)
    (official / "bar.toml").write_text(
        'id = "bitty-terminal.bar"\n'
        'name = "Bar"\n'
        'repository = "https://github.com/bitty-terminal/bar.git"\n'
        f'manifest_hash = "{MANIFEST_HASH}"\n'
        'description = "Catalog generator fixture"\n'
        'license = "MIT"\n'
        "\n"
        "[compatibility]\n"
        'bitty = ">=0.0.1,<1.0"\n',
        encoding="utf-8",
    )
    # A submodule pin has no worktree to add, so write the gitlink directly.
    # Stage the files first: `git add -A` afterwards would drop the gitlink
    # again because its path does not exist in the worktree.
    run_git(root, "add", "-A")
    run_git(
        root,
        "update-index",
        "--add",
        "--cacheinfo",
        f"160000,{GITLINK_PIN},plugins/bar",
    )
    run_git(root, "commit", "-q", "-m", "registry fixture")
    revision = run_git(root, "rev-parse", "HEAD")
    run_git(
        root,
        "remote",
        "add",
        "origin",
        "https://github.com/bitty-terminal/bitty-plugins.git",
    )
    return revision


def advance_gitlink(root: Path, new_pin: str) -> str:
    """Move the fixture's ``plugins/bar`` gitlink without touching metadata."""
    run_git(
        root,
        "update-index",
        "--add",
        "--cacheinfo",
        f"160000,{new_pin},plugins/bar",
    )
    run_git(root, "commit", "-q", "-m", "advance plugin pin only")
    return run_git(root, "rev-parse", "HEAD")


@unittest.skipUnless(shutil.which("git"), "git is required")
class GitRepositoryOverrideTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="bitty-catalog-test-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.registry = self.root / "registry"
        self.registry_revision = build_registry(self.registry)
        self.foreign = self.root / "foreign"
        self.foreign_revision = build_foreign_repo(self.foreign)
        self.assertNotEqual(self.foreign_revision, self.registry_revision)
        self.output = self.root / "official_catalog.rs"

    def polluted_env(self) -> dict[str, str]:
        """The caller's environment with every repository-location override."""
        foreign_git = self.foreign / ".git"
        env = dict(os.environ)
        env.update(
            {
                "GIT_DIR": str(foreign_git),
                "GIT_WORK_TREE": str(self.foreign),
                "GIT_COMMON_DIR": str(foreign_git),
                "GIT_INDEX_FILE": str(foreign_git / "index"),
                "GIT_OBJECT_DIRECTORY": str(foreign_git / "objects"),
                "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(foreign_git / "objects"),
                "GIT_PREFIX": "nested/",
            }
        )
        return env

    def run_generator(
        self, *args: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

    def generate(self, *, check: bool = False, env: dict[str, str] | None = None):
        command = [
            "--registry",
            str(self.registry),
            "--revision",
            self.registry_revision,
            "--output",
            str(self.output),
        ]
        if check:
            command.append("--check")
        return self.run_generator(*command, env=env)

    def test_git_dir_override_does_not_redirect_generation(self) -> None:
        result = self.generate(env=self.polluted_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        rendered = self.output.read_text(encoding="utf-8")
        self.assertIn(
            f'SOURCE_REVISION: &str = "{self.registry_revision}"',
            rendered,
        )
        self.assertIn('id: "bitty-terminal.bar"', rendered)
        self.assertNotIn(self.foreign_revision, rendered)

    def test_git_dir_override_does_not_redirect_check(self) -> None:
        generated = self.generate()
        self.assertEqual(generated.returncode, 0, generated.stderr)

        checked = self.generate(check=True, env=self.polluted_env())
        self.assertEqual(checked.returncode, 0, checked.stderr)
        self.assertIn("1 entries", checked.stdout)
        self.assertIn(self.registry_revision, checked.stdout)

        # The drift gate still reads the intended registry: a stale output is
        # reported as stale, not as a git failure from the foreign repository.
        self.output.write_text(
            self.output.read_text(encoding="utf-8") + "\n// drift\n",
            encoding="utf-8",
        )
        stale = self.generate(check=True, env=self.polluted_env())
        self.assertEqual(stale.returncode, 1, stale.stdout)
        self.assertIn("is stale", stale.stderr)

    def test_pin_only_registry_change_fails_check_pins(self) -> None:
        """Issue #2775: a code-only pin move is caught even when the manifest
        hash (and therefore digest verification) is unchanged."""
        generated = self.generate()
        self.assertEqual(generated.returncode, 0, generated.stderr)

        new_pin = "9" * 40
        advanced = advance_gitlink(self.registry, new_pin)

        # `--check` at the new revision would also fail, but it reports the
        # whole rendered file. `--check-pins` names the pin that moved.
        stale = self.run_generator(
            "--registry",
            str(self.registry),
            "--revision",
            advanced,
            "--output",
            str(self.output),
            "--check-pins",
        )
        self.assertEqual(stale.returncode, 1, stale.stdout)
        self.assertIn("bitty-terminal.bar", stale.stderr)
        self.assertIn(new_pin, stale.stderr)
        self.assertIn(GITLINK_PIN, stale.stderr)

        # The same check passes at the revision the catalog was rendered from.
        current = self.run_generator(
            "--registry",
            str(self.registry),
            "--revision",
            self.registry_revision,
            "--output",
            str(self.output),
            "--check-pins",
        )
        self.assertEqual(current.returncode, 0, current.stderr)

        # Regenerating at the advanced revision clears the drift.
        regenerated = self.run_generator(
            "--registry",
            str(self.registry),
            "--revision",
            advanced,
            "--output",
            str(self.output),
        )
        self.assertEqual(regenerated.returncode, 0, regenerated.stderr)
        refreshed = self.run_generator(
            "--registry",
            str(self.registry),
            "--revision",
            advanced,
            "--output",
            str(self.output),
            "--check-pins",
        )
        self.assertEqual(refreshed.returncode, 0, refreshed.stderr)

    def test_explicit_missing_registry_fails_without_fallback(self) -> None:
        """A requested --registry is authoritative, even if another tree exists."""
        missing = self.root / "absent-registry"
        env = dict(os.environ)
        env["BITTY_PLUGINS_DIR"] = str(self.registry)

        result = self.run_generator(
            "--registry",
            str(missing),
            "--revision",
            self.registry_revision,
            "--output",
            str(self.output),
            "--check",
            env=env,
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("requested registry", result.stderr)
        self.assertIn("via --registry", result.stderr)
        self.assertIn(str(missing), result.stderr)
        self.assertIn("no registry/official directory", result.stderr)
        self.assertFalse(self.output.exists())

    def test_requested_env_registry_fails_without_fallback(self) -> None:
        """BITTY_PLUGINS_DIR is authoritative over $BITTY_WORKSPACE discovery."""
        missing = self.root / "absent-env-registry"
        workspace = self.root / "workspace"
        workspace.mkdir()
        shutil.copytree(self.registry, workspace / "bitty-plugins")
        env = dict(os.environ)
        env["BITTY_PLUGINS_DIR"] = str(missing)
        env["BITTY_WORKSPACE"] = str(workspace)

        result = self.run_generator(
            "--revision",
            self.registry_revision,
            "--output",
            str(self.output),
            "--check",
            env=env,
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("requested registry", result.stderr)
        self.assertIn("via BITTY_PLUGINS_DIR", result.stderr)
        self.assertIn(str(missing), result.stderr)
        self.assertIn("unset BITTY_PLUGINS_DIR", result.stderr)


if __name__ == "__main__":
    unittest.main()
