# SPDX-FileCopyrightText: 2026-present Jarl Haggerty <Jarl.Haggerty@pennmedicine.upenn.edu>
#
# SPDX-License-Identifier: MIT


import pathlib
#import ext_task
import platform
import importlib
import importlib.resources

from thalamus.pipeline.thalamus_window import Factory, UserData, UserDataType

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
