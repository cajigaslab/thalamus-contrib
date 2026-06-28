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
    build_type = 'Debug' if debug else 'Release'

    sanitizer = os.environ.get('THALAMUS_SANITIZER', None)
    jobs = os.environ.get('THALAMUS_JOBS', str(os.cpu_count()))
    strip = os.environ.get('THALAMUS_STRIP', 'OFF')
    do_config = os.environ.get('THALAMUS_CONFIG', 'OFF') == 'ON'
    vv = os.environ.get('THALAMUS_VV', 'OFF') == 'ON'

    print('debug', debug)
    print('build_type', build_type)
    print('do_config', do_config)

    source_path = pathlib.Path(self.root)
    build_path = source_path / 'build' / build_type
    if sanitizer:
      build_path.with_name(build_path.name + '-' + sanitizer)
    osx_target = '12.0'

    cmake_command = [
      'cmake',
      '-S', source_path,
      '-B', build_path,
      f'-DCMAKE_BUILD_TYPE={build_type}',
      '-DCMAKE_POLICY_VERSION_MINIMUM=3.5',
      f'-DCMAKE_OSX_DEPLOYMENT_TARGET={osx_target}',
    ]
    cmake_command += ['-G', 'Ninja']

    if sanitizer:
      cmake_command += [f'-DSANITIZER={sanitizer}']
    if strip:
      cmake_command += [f'-DSTRIP=ON']

    cmake_command = [str(c) for c in cmake_command]
    print(cmake_command)
    if not (build_path / 'CMakeCache.txt').exists() or do_config:
      subprocess.check_call(cmake_command)

    command = ['cmake', '--build', build_path, '--config', build_type, '--parallel', jobs]

    command = [str(c) for c in command]
    print(command)
    subprocess.check_call(command)

    rust_dir = pathlib.Path(self.root) / "rust"
    cargo_command = ['cargo', 'build']
    if build_type == 'Release':
      cargo_command += ['--release']
    if vv:
      cargo_command += ['-vv']
    cargo_env = dict(os.environ)
    cargo_env['OPENCV_LINK_PATHS'] = str(build_path / '_deps' / 'opencv-build' / build_type / 'install' / 'staticlib')
    cargo_env['OPENCV_INCLUDE_PATHS'] = str(build_path / '_deps' / 'opencv-build' / build_type / 'install' / 'include')


    if platform.system() == 'Windows':
      d = 'd' if debug else ''
      opencv_libs = [
        'ipphal',
        'ippicvmt',
        f'IlmImf{d}',
        f'ippiw{d}',
        f'ittnotify{d}',
        f'libjpeg-turbo{d}',
        f'libopenjp2{d}',
        f'libpng{d}',
        f'libtiff{d}',
        f'opencv_calib3d4120{d}',
        f'opencv_core4120{d}',
        f'opencv_features2d4120{d}',
        f'opencv_flann4120{d}',
        f'opencv_imgcodecs4120{d}',
        f'opencv_imgproc4120{d}',
        f'opencv_objdetect4120{d}',
        f'opencv_stitching4120{d}',
        f'zlib{d}',
      ]
      cargo_env['OPENCV_LINK_LIBS'] = ','.join(opencv_libs)

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

