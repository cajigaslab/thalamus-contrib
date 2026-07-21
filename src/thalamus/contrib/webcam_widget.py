import json
import logging
from thalamus.qt import *
from thalamus.task_controller.util import create_task_with_exc_handling
from thalamus import thalamus_pb2

LOGGER = logging.getLogger(__name__)

class WebcamComboBox(QComboBox):
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
      return super().setCurrentText(text);
    for i in range(self.count()):
      if self.itemText(i) == text:
        super().setCurrentText(text)
        return

    self.addItem(text)
    super().setCurrentText(text)

  def showPopup(self):
    LOGGER.debug('showPopup')
    create_task_with_exc_handling(self.asyncShowPopup())

class WebcamWidget(QWidget):
  def __init__(self, config, stub):
    super().__init__()
    self.config = config
    self.stub = stub

    if 'Width' not in config:
      config['Width'] = 640
    if 'Height' not in config:
      config['Height'] = 480

    layout = QVBoxLayout()

    self.camera_combobox = WebcamComboBox(config, stub)
    self.camera_combobox.currentTextChanged.connect(lambda new_camera: config.update({"Camera": new_camera}))
    layout.addWidget(self.camera_combobox)

    self.running_checkbox = QCheckBox('Running')
    self.running_checkbox.toggled.connect(lambda value: config.update({'Running': value}))
    layout.addWidget(self.running_checkbox)

    layout.addWidget(QLabel('Width:'))
    self.width_spinbox = QSpinBox()
    self.width_spinbox.setRange(1, 10000)
    self.width_spinbox.setValue(config['Width'])
    self.width_spinbox.editingFinished.connect(lambda: config.update({'Width': self.width_spinbox.value()}))
    layout.addWidget(self.width_spinbox)

    layout.addWidget(QLabel('Height:'))
    self.height_spinbox = QSpinBox()
    self.height_spinbox.setRange(1, 10000)
    self.height_spinbox.setValue(config['Height'])
    self.height_spinbox.editingFinished.connect(lambda: config.update({'Height': self.height_spinbox.value()}))
    layout.addWidget(self.height_spinbox)

    async def test_resolution():
      name = self.config['name']
      response = await self.stub.node_request(thalamus_pb2.NodeRequest(node=name, json="\"test_resolution\""))
      result = json.loads(response.json)
      LOGGER.debug('test_resolution %s', result)
      success = isinstance(result, dict) and result.get('success')
      if success:
        message = f"{self.width_spinbox.value()}x{self.height_spinbox.value()} is supported by the selected camera."
      else:
        error = result.get('error') if isinstance(result, dict) else None
        message = f"{self.width_spinbox.value()}x{self.height_spinbox.value()} is not supported: {error or 'unknown error'}"

      formats = result.get('formats') if isinstance(result, dict) else None
      if formats:
        message += f"\n\nFormats supported by this camera:\n{formats}"

      if success:
        QMessageBox.information(self, 'Test Resolution', message)
      else:
        QMessageBox.warning(self, 'Test Resolution', message)

    def sync_test_resolution():
      create_task_with_exc_handling(test_resolution())

    self.test_button = QPushButton('Test')
    self.test_button.clicked.connect(sync_test_resolution)
    layout.addWidget(self.test_button)

    layout.addStretch(1)

    self.setLayout(layout)

    self.config.add_recursive_observer(self.on_change, lambda: isdeleted(self))
    self.config.recap(lambda *args: self.on_change(self.config, *args))

  def on_change(self, source, action, key, value):
    if key == 'Camera':
      self.camera_combobox.setCurrentText(value)
    elif key == 'Running':
      if self.running_checkbox.isChecked() != value:
        self.running_checkbox.setChecked(value)
    elif key == 'Width':
      if self.width_spinbox.value() != value:
        self.width_spinbox.setValue(value)
    elif key == 'Height':
      if self.height_spinbox.value() != value:
        self.height_spinbox.setValue(value)
