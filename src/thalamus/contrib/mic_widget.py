import json
import logging
from thalamus.qt import *
from thalamus.task_controller.util import create_task_with_exc_handling
from thalamus import thalamus_pb2

LOGGER = logging.getLogger(__name__)

DEFAULT_DEVICE = {'id': '', 'name': 'Default'}
FORMAT_KEYS = ('channels', 'sample_rate')

def format_text(mic_format):
  return f'{mic_format["channels"]} ch @ {mic_format["sample_rate"]} Hz'

def same_format(a, b):
  return all(a.get(k) == b.get(k) for k in FORMAT_KEYS)

class FormatComboBox(QComboBox):
  """Lists the formats reported by get_formats for the selected device."""
  def __init__(self, config, stub):
    super().__init__()
    self.stub = stub
    self.config = config
    self.request_id = 0
    self.currentIndexChanged.connect(self.on_index_changed)

  def on_index_changed(self, index):
    mic_format = self.itemData(index)
    if mic_format is not None and not same_format(mic_format, self.config.get('Format', {})):
      self.config['Format'] = mic_format

  def load_formats(self, device):
    self.request_id += 1
    create_task_with_exc_handling(self.async_load_formats(device, self.request_id))

  async def async_load_formats(self, device, request_id):
    device_id = device.get('id', '') if device is not None else ''
    request = json.dumps({'type': 'get_formats', 'id': device_id})
    response = await self.stub.node_request(thalamus_pb2.NodeRequest(node=self.config['name'], json=request))
    formats = json.loads(response.json) or []
    # The device changed again while this request was in flight
    if request_id != self.request_id:
      return

    self.blockSignals(True)
    try:
      self.clear()
      for mic_format in formats:
        self.addItem(format_text(mic_format), mic_format)
    finally:
      self.blockSignals(False)
    self.setFormat(self.config.get('Format', None))

  def setFormat(self, mic_format):
    self.blockSignals(True)
    try:
      if mic_format is None:
        self.setCurrentIndex(-1)
        return
      for i in range(self.count()):
        if same_format(self.itemData(i), mic_format):
          self.setCurrentIndex(i)
          return
      # Keep showing the configured format even if the device doesn't list it,
      # the node falls back to the device's default format. Without a
      # configured format the node uses the device's default too.
      self.addItem(format_text(mic_format), {k: mic_format[k] for k in FORMAT_KEYS})
      self.setCurrentIndex(self.count() - 1)
    finally:
      self.blockSignals(False)

class DeviceComboBox(QComboBox):
  """Lists the input devices reported by get_devices, loaded when opened."""
  def __init__(self, config, stub):
    super().__init__()
    self.stub = stub
    self.config = config
    self.loaded = False

  async def asyncShowPopup(self):
    if self.loaded:
      super().showPopup()
      return

    response = await self.stub.node_request(thalamus_pb2.NodeRequest(node=self.config['name'], json='"get_devices"'))
    devices = json.loads(response.json) or []
    current_device = self.config.get('Device', DEFAULT_DEVICE)
    self.blockSignals(True)
    try:
      self.clear()
      for device in [DEFAULT_DEVICE] + devices:
        self.addItem(device['name'], device)
    finally:
      self.blockSignals(False)
    self.setDevice(current_device)
    self.loaded = True
    super().showPopup()

  def setDevice(self, device):
    if device is None:
      return
    self.blockSignals(True)
    try:
      for i in range(self.count()):
        if self.itemData(i)['id'] == device['id']:
          self.setCurrentIndex(i)
          return
      self.addItem(device['name'], {'id': device['id'], 'name': device['name']})
      self.setCurrentIndex(self.count() - 1)
    finally:
      self.blockSignals(False)

  def showPopup(self):
    create_task_with_exc_handling(self.asyncShowPopup())

class MicWidget(QWidget):
  def __init__(self, config, stub):
    super().__init__()
    self.config = config
    self.stub = stub

    if 'Running' not in config:
      config['Running'] = False
    if 'Device' not in config:
      config['Device'] = dict(DEFAULT_DEVICE)

    config.add_recursive_observer(self.on_change, lambda: isdeleted(self))

    layout = QVBoxLayout()

    self.device_combobox = DeviceComboBox(config, stub)
    self.device_combobox.currentIndexChanged.connect(self.on_device_selected)
    layout.addWidget(self.device_combobox)

    self.running_checkbox = QCheckBox('Running')
    self.running_checkbox.toggled.connect(lambda value: config.update({'Running': value}))
    layout.addWidget(self.running_checkbox)

    layout.addWidget(QLabel('Format:'))
    self.format_combobox = FormatComboBox(config, stub)
    layout.addWidget(self.format_combobox)

    layout.addStretch(1)

    self.setLayout(layout)

    self.config.recap(lambda *args: self.on_change(self.config, *args))

  def on_device_selected(self, index):
    device = self.device_combobox.itemData(index)
    if device is not None and device['id'] != self.config.get('Device', {}).get('id'):
      self.config['Device'] = device

  def on_change(self, source, action, key, value):
    if source is self.config:
      if key == 'Device':
        self.device_combobox.setDevice(value)
        self.format_combobox.load_formats(value)
      elif key == 'Running':
        if self.running_checkbox.isChecked() != value:
          self.running_checkbox.setChecked(value)
      elif key == 'Format':
        self.format_combobox.setFormat(value)
    elif source is self.config.get('Device', None):
      if key == 'id':
        self.device_combobox.setDevice(source)
        self.format_combobox.load_formats(source)
    elif source is self.config.get('Format', None):
      self.format_combobox.setFormat(source)
