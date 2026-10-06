# SPDX-FileCopyrightText: 2026-present Jarl Haggerty <Jarl.Haggerty@pennmedicine.upenn.edu>
#
# SPDX-License-Identifier: MIT


import pathlib
#import ext_task
import platform
import importlib
import importlib.resources

from thalamus.pipeline.thalamus_window import Factory, UserData, UserDataType, get_node_names

from .thorcam_widget import ThorcamWidget
from .webcam_widget import WebcamWidget
from .mic_widget import MicWidget

print(__package__, type(__package__))
print(type(importlib.resources.files(__package__)))
#importlib.resources.files

def widgets():
  return {
    'JOYSTICK': Factory(None, [
      UserData(UserDataType.OPEN_FILE, 'Port', '/dev/ttyACM0', []),
      UserData(UserDataType.CHECK_BOX, 'Invert X', True, []),
      UserData(UserDataType.CHECK_BOX, 'Invert Y', False, []),
      UserData(UserDataType.SPINBOX, 'X Center', 516, []),
      UserData(UserDataType.SPINBOX, 'Y Center', 514, []),
      UserData(UserDataType.SPINBOX, 'Dead Zone', 3, []),
      UserData(UserDataType.CHECK_BOX, 'Running', False, []),
    ]),
    'THORCAM': Factory(lambda c, s: ThorcamWidget(c, s), [
      UserData(UserDataType.CHECK_BOX, 'Running', False, []),
      UserData(UserDataType.CHECK_BOX, 'View', False, []),
    ]),
    'WEBCAM': Factory(lambda c, s: WebcamWidget(c, s), [
      UserData(UserDataType.CHECK_BOX, 'Running', False, []),
      UserData(UserDataType.CHECK_BOX, 'View', False, []),
    ]),
    'MIC': Factory(lambda c, s: MicWidget(c, s), [
      UserData(UserDataType.CHECK_BOX, 'Running', False, []),
    ]),
    'RTMPS': Factory(None, [
      UserData(UserDataType.CHECK_BOX, 'Running', False, []),
      UserData(UserDataType.COMBO_BOX, 'Source', '', get_node_names),
      UserData(UserDataType.DEFAULT, 'Destination', '', []),
    ]),
    'ANGULAR_SCALING': Factory(None, [
      UserData(UserDataType.COMBO_BOX, 'Source', '', get_node_names),
      UserData(UserDataType.DOUBLE_SPINBOX, 'Fixation X', 0.0, []),
      UserData(UserDataType.DOUBLE_SPINBOX, 'Fixation Y', 0.0, []),
    ]),
    'MEDIA_CONVERTER': Factory(None, [
      UserData(UserDataType.COMBO_BOX, 'Source', '', get_node_names),
      UserData(UserDataType.COMBO_BOX, 'Video Format', 'RGB', [
        'PASSTHROUGH', 'Gray', 'RGB', 'YUYV422', 'YUV420P', 'YUVJ420P', 'NV12', 'BGR', 'MPEG4'
      ]),
      UserData(UserDataType.SPINBOX, 'Video Quality', 5, []),
      UserData(UserDataType.SPINBOX, 'Video Width', 0, []),
      UserData(UserDataType.SPINBOX, 'Video Height', 0, []),
      UserData(UserDataType.CHECK_BOX, 'Complete Frames', False, []),
      UserData(UserDataType.COMBO_BOX, 'Audio Format', 'PASSTHROUGH', [
        'PASSTHROUGH', 'integer', 'decimal', 'AAC'
      ]),
      # kbit/s for the whole stream, 0 for 64 kbit/s
      UserData(UserDataType.SPINBOX, 'Audio Bit Rate', 0, []),
      # Hz, 0 to keep the source's sample rate
      UserData(UserDataType.SPINBOX, 'Audio Sample Rate', 0, []),
      # Channel to start selecting input channels at: 0, 1, ... count from the
      # first channel forwards, -1, -2, ... from the last channel backwards
      UserData(UserDataType.SPINBOX, 'Audio Index', -1, []),
      UserData(UserDataType.CHECK_BOX, 'View', False, []),
    ]),
    'SLEEVE': Factory(None, [
      UserData(UserDataType.CHECK_BOX, 'Running', False, []),
      UserData(UserDataType.DEFAULT, 'Address', '', []),
    ]),
  }

def library():
  if platform.system() == 'Windows':
    return importlib.resources.files(__package__) / 'thalamus_contrib.dll'
    #return pathlib.Path('C:/thalamus-contrib/rust/target/debug/thalamus_contrib.dll')
  elif platform.system() == 'Darwin':
    return importlib.resources.files(__package__) / 'libthalamus_contrib.dylib'
  else:
    return importlib.resources.files(__package__) / 'libthalamus_contrib.so'

def tasks():
  return []
