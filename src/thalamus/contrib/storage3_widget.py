# SPDX-FileCopyrightText: 2026-present Jarl Haggerty <Jarl.Haggerty@pennmedicine.upenn.edu>
#
# SPDX-License-Identifier: MIT


from thalamus.pipeline.thalamus_window import Factory, UserData, UserDataType, get_node_names

def widgets():
  return {
    'STORAGE3': Factory(None, [
      UserData(UserDataType.CHECK_BOX, 'Running', False, []),
      UserData(UserDataType.DEFAULT, 'Output', '', []),
      UserData(UserDataType.DEFAULT, 'Sources', '', []),
    ]),
  }