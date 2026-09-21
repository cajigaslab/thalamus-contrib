import json
import logging
from thalamus.qt import *
from thalamus.task_controller.util import create_task_with_exc_handling
from thalamus import thalamus_pb2

LOGGER = logging.getLogger(__name__)

VIEW_ROTATIONS = {
  '0': 0,
  '90': 90,
  '180': 180,
  '270': 270,
  0: 0,
  90: 90,
  180: 180,
  270: 270,
}

WIDTHS = (320, 640, 800, 1024, 1280, 1600, 1920, 2560, 3840)
HEIGHTS = (240, 480, 600, 720, 768, 900, 1080, 1440, 2160)

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
    current_camera = self.config.get('Camera', {})

    response = await self.stub.node_request(thalamus_pb2.NodeRequest(node=name,json="\"get_cameras\""))
    print('get_cameras', response)
    cameras = json.loads(response.json)
    LOGGER.debug('asyncShowPopup %s', cameras)
    self.clear()
    if cameras is None:
      return
    for camera in cameras:
      self.addItem(f'{camera["index"]}: {camera["name"]}', camera)
    for i in range(self.count()):
      if self.itemData(i)['index'] == current_camera.get('index', None):
        self.setCurrentIndex(i)
        self.config['Camera'] = self.itemData(i)
        break
    self.loaded = True
    super().showPopup()

  def setCamera(self, camera):
    print('setCamera', camera)
    if camera is None:
      return
    
    for i in range(self.count()):
      if self.itemData(i)['index'] == camera['index']:
        super().setCurrentIndex(i)
        return

    text = f'{camera["index"]}: {camera["name"]}'
    self.addItem(text, camera)
    super().setCurrentText(text)

  def showPopup(self):
    LOGGER.debug('showPopup')
    create_task_with_exc_handling(self.asyncShowPopup())

class WebcamWidget(QWidget):
  def __init__(self, config, stub):
    super().__init__()
    self.config = config
    self.stub = stub

    if 'Running' not in config:
      config['Running'] = False
    if 'Width' not in config:
      config['Width'] = 640
    if 'Height' not in config:
      config['Height'] = 480
    if 'AcquisitionFrameRate' not in config:
      config['AcquisitionFrameRate'] = 30
    if 'Camera' not in config:
      config['Camera'] = {'index': -1, 'name': 'NULL', 'description': 'NULL'}
    self.camera = config['Camera']

    config.add_recursive_observer(self.on_change, lambda: isdeleted(self))

    layout = QVBoxLayout()

    self.camera_combobox = WebcamComboBox(config, stub)
    self.camera_combobox.currentIndexChanged.connect(lambda index: config.update({"Camera": self.camera_combobox.itemData(index)}))
    layout.addWidget(self.camera_combobox)

    self.running_checkbox = QCheckBox('Running')
    self.running_checkbox.toggled.connect(lambda value: config.update({'Running': value}))
    layout.addWidget(self.running_checkbox)

    layout.addWidget(QLabel('Width:'))
    self.width_combobox = QSpinBox()
    self.width_combobox.setRange(0, 1000000)
    self.width_combobox.valueChanged.connect(
      lambda _: config.update({'Width': self.width_combobox.value()}))
    layout.addWidget(self.width_combobox)

    layout.addWidget(QLabel('Height:'))
    self.height_combobox = QSpinBox()
    self.height_combobox.setRange(0, 1000000)
    self.height_combobox.valueChanged.connect(
      lambda _: config.update({'Height': self.height_combobox.value()}))
    layout.addWidget(self.height_combobox)

    layout.addWidget(QLabel('Frame Rate:'))
    self.framerate_spinbox = QDoubleSpinBox()
    self.framerate_spinbox.setRange(0, 1000000)
    self.framerate_spinbox.setSuffix(' Hz')
    self.framerate_spinbox.editingFinished.connect(lambda: config.update({'AcquisitionFrameRate': self.framerate_spinbox.value()}))
    layout.addWidget(self.framerate_spinbox)

    layout.addStretch(1)

    self.setLayout(layout)

    self.config.recap(lambda *args: self.on_change(self.config, *args))

  def on_change(self, source, action, key, value):
    if source is self.config:
      if key == 'Camera':
        self.camera_combobox.setCamera(value)
      elif key == 'Running':
        if self.running_checkbox.isChecked() != value:
          self.running_checkbox.setChecked(value)
      elif key == 'Width':
        self.width_combobox.setValue(int(value))
      elif key == 'Height':
        self.height_combobox.setValue(int(value))
      elif key == 'AcquisitionFrameRate':
        if abs(self.framerate_spinbox.value() - value) >= 1:
          self.framerate_spinbox.setValue(value)
    elif source is self.camera:
      self.camera_combobox.setCamera(source)
