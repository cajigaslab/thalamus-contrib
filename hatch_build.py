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
    build_data["pure_python"] = False   # marks it platform-specific
    build_data["infer_tag"] = False      # don't auto-detect from this machine
    
    osx_target = '12.0'
    osx_target_underscored = osx_target.replace('.', '_')
    platform_tag = None
    if platform.system() == 'Windows':
      platform_tag = 'win_amd64'
    elif platform.system() == 'Darwin':
      processor = 'arm64' if 'arm' in platform.processor() else 'x86_64'
      platform_tag = f'macosx_{osx_target_underscored}_{processor}'
    elif platform.system() == 'Linux':
      ldd_output = subprocess.check_output(['ldd', '--version'], encoding='utf8')
      assert ldd_output is not None
      ldd_line = [l.strip() for l in ldd_output.split('\n') if l[:3] == 'ldd'][0]
      libc_version = ldd_line.split(' ')[-1]
      platform_tag = f'manylinux_{libc_version.replace(".", "_")}_x86_64'
    assert platform_tag is not None
    build_data["tag"] = f'py3-none-{platform_tag}'

    pprint.pprint({k: v for k, v in os.environ.items() if k.startswith('THALAMUS_')})
    
    debug = os.environ.get('THALAMUS_DEBUG', 'OFF') == 'ON'
    vv = os.environ.get('THALAMUS_VV', 'OFF') == 'ON'
    do_config = os.environ.get('THALAMUS_CONFIG', 'OFF') == 'ON'

    default_parallel = str(os.cpu_count())
    parallel = os.environ.get('THALAMUS_JOB', default_parallel)
    is_android = False

    print('debug', debug)
    build_path = pathlib.Path.cwd() / 'build' / f'{"android-" if is_android else ""}{"debug" if debug else "release"}'
    osx_target = '12.0'

    cmake_command = [
      'cmake',
      '-S', pathlib.Path.cwd(),
      '-B', build_path,
      f'-DCMAKE_BUILD_TYPE={"Debug" if debug else "Release"}',
      '-DCMAKE_EXPORT_COMPILE_COMMANDS=ON',
      '-DCMAKE_POLICY_VERSION_MINIMUM=3.5',
      f'-DCMAKE_OSX_DEPLOYMENT_TARGET={osx_target}',
      '-DCMAKE_C_COMPILER=clang',
      '-DCMAKE_CXX_COMPILER=clang++',
      '-DCMAKE_LINKER=clang',
      '-DCMAKE_MAKE_PROGRAM=' + shutil.which('ninja'),
      '-G', 'Ninja',
    ]

    if platform.system() == 'Linux':
      cmake_command += ['-DCMAKE_LIBRARY_ARCHITECTURE=x86_64-linux-gnu']

    cmake_command = [str(c) for c in cmake_command]
    print(cmake_command)
    if not (build_path / 'CMakeCache.txt').exists() or do_config:
      subprocess.check_call(cmake_command)
    #shutil.copy(build_path / 'compile_commands.json', 'compile_commands.json')
    
    command = ['cmake', '--build', build_path, '--config', "Debug" if debug else "Release", '--parallel', parallel]

    command = [str(c) for c in command]
    print(command)
    subprocess.check_call(command)

    rust_dir = pathlib.Path(self.root) / "rust"
    cargo_command = ['cargo', 'build']
    if not debug:
      cargo_command += ['--release']
    if vv:
      cargo_command += ['-vv']

    ffmpeg_install = build_path / '_deps' / 'ffmpeg-build' / ("Debug" if debug else "Release") / 'install'
    cargo_env = dict(os.environ)
    cargo_env['FFMPEG_DIR'] = str(ffmpeg_install)
    cargo_env['PKG_CONFIG_PATH'] = str(ffmpeg_install / 'lib' / 'pkgconfig')
    if platform.system() == 'Darwin':
      # Match the deployment target the ffmpeg/CMake deps were built against so
      # rustc (and the cc-crate built C/C++ deps like imgui-sys) stamp the same
      # minimum OS version and the linker stops warning about objects built for
      # a newer macOS than is being linked.
      cargo_env['MACOSX_DEPLOYMENT_TARGET'] = osx_target

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

