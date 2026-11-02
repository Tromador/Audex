"""Audex reuses the canonical package with a real entry point and offline inputs."""

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from codex_package import cargo, cli, v8
from codex_package.targets import PACKAGE_VARIANTS, TARGET_SPECS


class AudexPackageTest(unittest.TestCase):
    def test_real_regular_entrypoint_and_complete_companions(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "executable"
            source.write_bytes(b"local executable fixture")
            source.chmod(0o755)
            package = root / "package"
            args = cli.create_parser().parse_args(
                [
                    "--variant",
                    "audex",
                    "--target",
                    "x86_64-unknown-linux-gnu",
                    "--package-dir",
                    str(package),
                    "--entrypoint-bin",
                    str(source),
                    "--code-mode-host-bin",
                    str(source),
                    "--exec-server-bin",
                    str(source),
                    "--bwrap-bin",
                    str(source),
                    "--rg-bin",
                    str(source),
                    "--strip",
                    "none",
                ]
            )
            with patch.object(
                v8, "download_file", side_effect=AssertionError("network")
            ):
                cli.assemble_package(args)
            metadata = json.loads((package / "codex-package.json").read_text())
            self.assertEqual(metadata["entrypoint"], "bin/audex")
            self.assertEqual(metadata["variant"], "audex")
            for name in [
                "bin/audex",
                "bin/codex-code-mode-host",
                "bin/codex-exec-server",
                "codex-resources/bwrap",
                "codex-path/rg",
            ]:
                self.assertTrue((package / name).is_file())
                self.assertFalse((package / name).is_symlink())
                self.assertEqual((package / name).read_bytes(), source.read_bytes())

    def test_missing_ripgrep_is_refused_without_download(self):
        args = cli.create_parser().parse_args(["--variant", "audex"])
        with self.assertRaisesRegex(RuntimeError, "explicit local --rg-bin"):
            cli.assemble_package(args)

    def test_missing_v8_inputs_are_refused_without_download(self):
        with (
            tempfile.TemporaryDirectory() as temp,
            patch.object(v8, "download_file") as download,
        ):
            with self.assertRaisesRegex(RuntimeError, "offline package build"):
                v8.fetch_codex_v8_artifacts(
                    TARGET_SPECS["x86_64-unknown-linux-gnu"],
                    version="152.2.0",
                    cache_root=Path(temp),
                    offline=True,
                )
            download.assert_not_called()

    def test_audex_build_is_locked_and_offline(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name in ["audex", "codex-code-mode-host", "codex-exec-server", "bwrap"]:
                (root / name).write_bytes(b"fixture")
            with (
                patch.object(cargo, "cargo_profile_output_dir", return_value=root),
                patch.object(
                    cargo, "resolve_codex_v8_cargo_env", return_value={}
                ) as resolve,
                patch.object(cargo.subprocess, "run") as run,
            ):
                cargo.build_source_binaries(
                    TARGET_SPECS["x86_64-unknown-linux-gnu"],
                    PACKAGE_VARIANTS["audex"],
                    cargo="cargo",
                    profile="dev",
                    entrypoint_bin=None,
                    code_mode_host_bin=None,
                    exec_server_bin=None,
                    bwrap_bin=None,
                    codex_command_runner_bin=None,
                    codex_windows_sandbox_setup_bin=None,
                )
            command = run.call_args.args[0]
            self.assertIn("--locked", command)
            self.assertIn("--offline", command)
            self.assertIn("audex", command)
            self.assertNotIn("codex", command)
            resolve.assert_called_once_with(
                TARGET_SPECS["x86_64-unknown-linux-gnu"], offline=True
            )
