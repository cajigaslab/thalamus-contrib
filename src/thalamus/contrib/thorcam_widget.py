import json
import logging
from thalamus.qt import *
from thalamus.task_controller.util import create_task_with_exc_handling
from thalamus import thalamus_pb2
from thalamus.pipeline.genicam_widget import RoiWidget

LOGGER = logging.getLogger(__name__)

class ThorcamComboBox(QComboBox):
  def __init__(self, config, stub):
    super().__init__()
    self.stub = stub
    self.config = config
    self.loaded = False

  async def asyncShowPopup(self):
    LOGGER.debug('asyncShowPopup')
    if self.loaded:
      super().showPopup()
      return

    name = self.config['name']
    current_camera = self.config.get('Camera', None)

    response = await self.stub.node_request(thalamus_pb2.NodeRequest(node=name,json="\"get_cameras\""))
    cameras = json.loads(response.json)
    LOGGER.debug('asyncShowPopup %s', cameras)
    self.clear()
    if cameras is None:
      return
    self.addItems(cameras)
    for i, v in enumerate(cameras):
      if v == current_camera:
        self.setCurrentIndex(i)
        break
    self.loaded = True
    super().showPopup()

  def setCurrentText(self, text):
    if self.loaded:
      return super().setCurrentText(text)
    for i in range(self.count()):
      if self.itemText(i) == text:
        super().setCurrentText(text)
        return

    self.addItem(text)
    super().setCurrentText(text)

  def showPopup(self):
    LOGGER.debug('showPopup')
    create_task_with_exc_handling(self.asyncShowPopup())

class ThorcamRoiWidget(RoiWidget):
  """RoiWidget edits the ROI in its config and draws the hardware ROI from
  config['Camera Values']. Thorcam keeps these in separate Desired and Actual
  dicts, so edit Desired and draw Actual."""
  def __init__(self, desired, actual):
    super().__init__(desired)
    self.camera_values = actual

    def on_change(*_):
      self.updateGeometry()
      self.update()

    actual.add_recursive_observer(on_change, lambda: isdeleted(self))

class ThorcamWidget(QWidget):
  def __init__(self, config, stub):
    super().__init__()
    self.config = config
    self.stub = stub

    if 'Running' not in config:
      config['Running'] = False
    if 'Desired' not in config:
      config['Desired'] = {}
    if 'Actual' not in config:
      config['Actual'] = {}
    self.desired = config['Desired']
    self.actual = config['Actual']

    config.add_recursive_observer(self.on_change, lambda: isdeleted(self))

    layout = QVBoxLayout()

    self.camera_combobox = ThorcamComboBox(config, stub)
    self.camera_combobox.currentTextChanged.connect(lambda new_camera: config.update({"Camera": new_camera}))
    layout.addWidget(self.camera_combobox)

    self.running_checkbox = QCheckBox('Running')
    self.running_checkbox.toggled.connect(lambda value: config.update({'Running': value}))
    layout.addWidget(self.running_checkbox)

    self.roi_widget = ThorcamRoiWidget(self.desired, self.actual)
    layout.addWidget(self.roi_widget)

    layout.addWidget(QLabel('Frame Rate:'))
    self.framerate_spinbox = QDoubleSpinBox()
    self.framerate_spinbox.setRange(0, 1000000)
    self.framerate_spinbox.setSuffix('Hz')
    self.framerate_spinbox.editingFinished.connect(lambda: self.desired.update({'AcquisitionFrameRate': self.framerate_spinbox.value()}))
    self.camera_framerate = QLabel()
    layout2 = QHBoxLayout()
    layout2.addWidget(self.framerate_spinbox)
    layout2.addWidget(self.camera_framerate)
    layout.addLayout(layout2)

    layout.addWidget(QLabel('Exposure:'))
    self.exposure_spinbox = QDoubleSpinBox()
    self.exposure_spinbox.setRange(0, 1000000)
    self.exposure_spinbox.setSuffix('ms')
    self.exposure_spinbox.editingFinished.connect(lambda: self.desired.update({'ExposureTime': 1000*self.exposure_spinbox.value()}))
    self.camera_exposure = QLabel()
    layout2 = QHBoxLayout()
    layout2.addWidget(self.exposure_spinbox)
    layout2.addWidget(self.camera_exposure)
    layout.addLayout(layout2)

    layout.addWidget(QLabel('Gain:'))
    self.gain_spinbox = QDoubleSpinBox()
    self.gain_spinbox.setRange(0, 1000000)
    self.gain_spinbox.editingFinished.connect(lambda: self.desired.update({'Gain': self.gain_spinbox.value()}))
    self.camera_gain = QLabel()
    layout2 = QHBoxLayout()
    layout2.addWidget(self.gain_spinbox)
    layout2.addWidget(self.camera_gain)
    layout.addLayout(layout2)

    async def sync_config():
      name = self.config['name']
      await self.stub.node_request(thalamus_pb2.NodeRequest(node=name,json="\"sync_config\""))

    def sync_sync_config():
      create_task_with_exc_handling(sync_config())

    self.sync_button = QPushButton('Sync')
    self.sync_button.clicked.connect(sync_sync_config)
    layout.addWidget(self.sync_button)

    layout.addStretch(1)

    self.setLayout(layout)

    self.config.recap(lambda *args: self.on_change(self.config, *args))
    self.desired.recap(lambda *args: self.on_change(self.desired, *args))
    self.actual.recap(lambda *args: self.on_change(self.actual, *args))

  def on_change(self, source, action, key, value):
    if source is self.actual:
      if key == 'AcquisitionFrameRate':
        self.camera_framerate.setText(f'{value} Hz')
      elif key == 'ExposureTime':
        self.camera_exposure.setText(f'{value*1e-3} ms')
      elif key == 'Gain':
        self.camera_gain.setText(f'{value}')
      return

    if source is self.desired:
      if key == 'AcquisitionFrameRate':
        if abs(self.framerate_spinbox.value() - value) >= 1:
          self.framerate_spinbox.setValue(value)
      elif key == 'ExposureTime':
        if abs(1000*self.exposure_spinbox.value() - value) >= 1:
          self.exposure_spinbox.setValue(.001*value)
      elif key == 'Gain':
        if abs(self.gain_spinbox.value() - value) >= 1:
          self.gain_spinbox.setValue(value)
      return

    if key == 'Running':
      if self.running_checkbox.isChecked() != value:
        self.running_checkbox.setChecked(value)
    elif key == 'Camera':
      self.camera_combobox.setCurrentText(value)
