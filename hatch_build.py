import platform
import subprocess
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface


class CustomBuildHook(BuildHookInterface):
    PLUGIN_NAME = "custom"

    def initialize(self, version, build_data):
        rust_dir = Path(self.root) / "rust"
        subprocess.run(["cargo", "build", "--release"], cwd=rust_dir, check=True)

        system = platform.system()
        if system == "Windows":
            lib_name = "thalamus_contrib.dll"
        elif system == "Darwin":
            lib_name = "libthalamus_contrib.dylib"
        else:
            lib_name = "libthalamus_contrib.so"

        lib_src = rust_dir / "target" / "release" / lib_name
        build_data["force_include"][str(lib_src)] = f"thalamus_contrib/{lib_name}"
