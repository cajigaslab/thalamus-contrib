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

FORMAT_KEYS = ('width', 'height', 'frame_rate', 'format')

def format_text(camera_format):
  return (f'{camera_format["width"]}x{camera_format["height"]} @ {camera_format["frame_rate"]} Hz '
          f'({camera_format["format"]})')

def same_format(a, b):
  return all(a.get(k) == b.get(k) for k in FORMAT_KEYS)

class FormatComboBox(QComboBox):
  """Lists the formats reported by get_formats for the selected camera."""
  def __init__(self, config, stub):
    super().__init__()
    self.stub = stub
    self.config = config
    self.request_id = 0
    self.currentIndexChanged.connect(self.on_index_changed)

  def on_index_changed(self, index):
    camera_format = self.itemData(index)
    if camera_format is not None and not same_format(camera_format, self.config.get('Format', {})):
      self.config['Format'] = camera_format

  def load_formats(self, camera):
    self.request_id += 1
    create_task_with_exc_handling(self.async_load_formats(camera, self.request_id))

  async def async_load_formats(self, camera, request_id):
    formats = []
    index = camera.get('index', -1) if camera is not None else -1
    if index >= 0:
      request = json.dumps({'type': 'get_formats', 'index': index})
      response = await self.stub.node_request(thalamus_pb2.NodeRequest(node=self.config['name'], json=request))
      formats = json.loads(response.json) or []
    # The camera changed again while this request was in flight
    if request_id != self.request_id:
      return

    self.blockSignals(True)
    try:
      self.clear()
      for camera_format in formats:
        self.addItem(format_text(camera_format), camera_format)
    finally:
      self.blockSignals(False)
    self.setFormat(self.config.get('Format', None))

  def setFormat(self, camera_format):
    self.blockSignals(True)
    try:
      if camera_format is None:
        self.setCurrentIndex(-1)
        return
      for i in range(self.count()):
        if same_format(self.itemData(i), camera_format):
          self.setCurrentIndex(i)
          return
      # Keep showing the configured format even if the camera doesn't list it,
      # the node will use the closest format the camera has.
      self.addItem(format_text(camera_format), {k: camera_format[k] for k in FORMAT_KEYS})
      self.setCurrentIndex(self.count() - 1)
    finally:
      self.blockSignals(False)

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
    if 'Format' not in config:
      config['Format'] = {
        'width': 640,
        'height': 480,
        'frame_rate': 30,
        'format': 'MJPEG',
      }
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

    layout.addWidget(QLabel('Format:'))
    self.format_combobox = FormatComboBox(config, stub)
    layout.addWidget(self.format_combobox)

    layout.addStretch(1)

    self.setLayout(layout)

    self.config.recap(lambda *args: self.on_change(self.config, *args))

  def on_change(self, source, action, key, value):
    if source is self.config:
      if key == 'Camera':
        self.camera = value
        self.camera_combobox.setCamera(value)
        self.format_combobox.load_formats(value)
      elif key == 'Running':
        if self.running_checkbox.isChecked() != value:
          self.running_checkbox.setChecked(value)
      elif key == 'Format':
        self.format_combobox.setFormat(value)
    elif source is self.camera:
      self.camera_combobox.setCamera(source)
      if key == 'index':
        self.format_combobox.load_formats(source)
    elif source is self.config.get('Format', None):
      self.format_combobox.setFormat(source)
