import os
import sys
import platform
import shutil
import subprocess
import pathlib
import pprint

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

class CustomBuildHook(BuildHookInterface):
  PLUGIN_NAME = "custom"

  def initialize(self, version, build_data):
    print('target_name', self.target_name)
    if self.target_name == 'sdist':
      return
    #print('files', pprint.pformat(list(pathlib.Path.cwd().rglob('*'))), file=sys.stderr)
    
    debug = os.environ.get('THALAMUS_DEBUG', 'OFF') == 'ON'
    vv = os.environ.get('THALAMUS_VV', 'OFF') == 'ON'

    print('debug', debug)

    rust_dir = pathlib.Path(self.root) / "rust"
    cargo_command = ['cargo', 'build']
    if not debug:
      cargo_command += ['--release']
    if vv:
      cargo_command += ['-vv']
    cargo_env = dict(os.environ)

    print('cargo_command', cargo_command)
    subprocess.run(cargo_command, cwd=rust_dir, check=True, env=cargo_env)

    system = platform.system()
    if system == "Windows":
      lib_name = "thalamus_contrib.dll"
    elif system == "Darwin":
      lib_name = "libthalamus_contrib.dylib"
    else:
      lib_name = "libthalamus_contrib.so"

    rust_target = 'debug' if debug else 'release'
    lib_src = rust_dir / "target" / rust_target / lib_name
    lib_dst = pathlib.Path(self.root) / "src" / "thalamus" / "contrib" / lib_name
    shutil.copy2(lib_src, lib_dst)

    if system == 'Windows':
      shutil.copy2(lib_src.with_suffix('.pdb'), lib_dst.with_suffix('.pdb'))

