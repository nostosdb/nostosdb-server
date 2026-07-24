import json
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "distribution" / "scripts"
sys.path.insert(0, str(SCRIPTS))

from common import executable_name, host_target, release_manifest


def invoke(*arguments):
    return subprocess.run(
        [str(value) for value in arguments],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


class NpmDistributionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = Path(tempfile.mkdtemp(prefix="nostd-npm-test-"))

    def tearDown(self):
        shutil.rmtree(self.temporary)

    def fake_binary(self, target):
        binary = self.temporary / executable_name(target)
        binary.write_bytes(b"native nostd fixture\n")
        binary.chmod(0o755)
        return binary

    def test_manifest_declares_six_exact_server_packages(self):
        manifest = release_manifest()
        self.assertEqual(manifest["version"], "0.0.3")
        self.assertEqual(
            set(manifest["targets"]),
            {
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "aarch64-pc-windows-msvc",
                "x86_64-pc-windows-msvc",
                "aarch64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu",
            },
        )
        packages = {
            details["npm_package"] for details in manifest["targets"].values()
        }
        self.assertEqual(len(packages), 6)
        self.assertTrue(all(name.startswith("@nostdb/server-") for name in packages))

    def test_candidate_scripts_do_not_publish(self):
        for path in sorted(SCRIPTS.glob("*.py")):
            self.assertNotIn("npm publish", path.read_text(encoding="utf-8"))

    def test_stages_unpublished_launcher_and_native_package(self):
        target = host_target()
        output = self.temporary / "output"
        binary = self.fake_binary(target)
        result = invoke(
            sys.executable,
            SCRIPTS / "stage_npm_candidate.py",
            "--target",
            target,
            "--binary",
            binary,
            "--output",
            output,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        self.assertFalse(payload["published"])
        self.assertEqual(payload["launcher"]["name"], "@nostdb/server")
        launcher = output / payload["launcher"]["filename"]
        platform = output / payload["platform"]["filename"]
        self.assertTrue(launcher.is_file())
        self.assertTrue(platform.is_file())
        for archive in (launcher, platform):
            with tarfile.open(archive, mode="r:gz") as package:
                contents = {
                    member.name: package.extractfile(member).read()
                    for member in package.getmembers()
                    if member.isfile()
                }
            self.assertEqual(contents["package/LICENSE"], (ROOT / "LICENSE").read_bytes())
            self.assertEqual(contents["package/NOTICE"], (ROOT / "NOTICE").read_bytes())
            self.assertEqual(contents["package/README.md"], (ROOT / "README.md").read_bytes())
            manifest = json.loads(contents["package/package.json"])
            self.assertNotIn("preinstall", manifest.get("scripts", {}))
            self.assertNotIn("install", manifest.get("scripts", {}))
            self.assertNotIn("postinstall", manifest.get("scripts", {}))
            if archive == platform:
                executable = "package/bin/{}".format(executable_name(target))
                self.assertEqual(contents[executable], binary.read_bytes())
            else:
                self.assertEqual(manifest["dependencies"], {"@nostdb/cli": "0.0.3"})
                self.assertEqual(set(manifest["bin"]), {"nostdb", "nostd"})

    def test_refuses_to_replace_an_existing_stage(self):
        target = host_target()
        output = self.temporary / "output"
        (output / "stage").mkdir(parents=True)
        result = invoke(
            sys.executable,
            SCRIPTS / "stage_npm_candidate.py",
            "--target",
            target,
            "--binary",
            self.fake_binary(target),
            "--output",
            output,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("refusing to replace npm stage", result.stderr)


if __name__ == "__main__":
    unittest.main()
