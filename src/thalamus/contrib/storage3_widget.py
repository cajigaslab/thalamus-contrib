"""STORAGE3's widget: STORAGE2's Sources and Metadata tables, and a list of
output URLs in place of its single output file."""
import time
import asyncio
import logging

from thalamus.qt import *
from thalamus.observable_item_models import TreeObservableCollectionModel, TreeObservableCollectionDelegate, get_selected_rows
from thalamus.task_controller.util import create_task_with_exc_handling

LOGGER = logging.getLogger(__name__)

class Storage3Widget(QWidget):
  def __init__(self, config, stub):
    super().__init__()
    config_parent = config.parent
    assert config_parent is not None

    status = QLabel()
    status.setText('WAITING')
    status.setStyleSheet('color: red')

    if 'Sources' not in config:
      config['Sources'] = []
    sources = config['Sources']

    if 'Outputs' not in config:
      config['Outputs'] = [{'URL': 'test.tha', 'Main': True, 'Hash URL': ''}]
    outputs = config['Outputs']
    for output in outputs:
      if 'Hash URL' not in output:
        output['Hash URL'] = ''

    if 'Metadata' not in config:
      config['Metadata'] = []
    metadata = config['Metadata']

    def node_choices(collection, key):
      if key == 'Node':
        return sorted(node['name'] for node in config_parent)

    def add_table(layout, label, collection, columns, new_row, delegate=None):
      model = TreeObservableCollectionModel(collection, key_column='#', columns=columns, show_extra_values=False,
                                            is_editable=lambda *arg: True)
      view = QTreeView()
      view.setModel(model)
      if delegate is not None:
        delegate = delegate(model)
        view.setItemDelegate(delegate)
      add_button = QPushButton('Add')
      remove_button = QPushButton('Remove')

      def on_add():
        collection.append(new_row())

      def on_remove():
        for item in get_selected_rows(view):
          del collection[item]

      add_button.clicked.connect(on_add)
      remove_button.clicked.connect(on_remove)

      layout.addWidget(QLabel(label))
      layout.addWidget(view)
      button_layout = QHBoxLayout()
      button_layout.addWidget(add_button)
      button_layout.addWidget(remove_button)
      layout.addLayout(button_layout)
      # The view doesn't keep the model and delegate alive.
      return model, view, delegate

    layout = QVBoxLayout()
    layout.addWidget(status)
    self.tables = [
      add_table(layout, 'Nodes:', sources, ['Node', 'Time Series', 'Image', 'Motion', 'Text'],
                lambda: {'Node': '', 'Time Series': True, 'Image': True, 'Motion': True, 'Text': True},
                lambda model: TreeObservableCollectionDelegate(model, 3, node_choices)),
      # URL: a path or file:// URL for a local file, or an http:// or
      # https:// URL to stream the recording to with a POST, e.g. Thoth's
      # https://HOST/files/DATA_DIR/PATH. The recording number is chosen for
      # the Main output and used for all of them; the node keeps exactly one
      # Main. Hash URL: empty, or a path or URL (rendered like URL) that gets
      # the output's Adler-32 once it finishes, e.g. Thoth's
      # https://HOST/hashes/DATA_DIR/PATH.
      add_table(layout, 'Outputs:', outputs, ['URL', 'Main', 'Hash URL'],
                lambda: {'URL': '', 'Main': False, 'Hash URL': ''}),
      add_table(layout, 'Metadata:', metadata, ['Key', 'Value'], lambda: {'Key': 'Key', 'Value': 'Value'}),
    ]
    self.setLayout(layout)
    self.task = None

    if 'rec' not in config:
      config['rec'] = 0

    if 'start' not in config:
      config['start'] = time.perf_counter()

    async def __status_loop():
      try:
        while True:
          rec = config['rec']
          start = config['start']
          elapsed = int(time.perf_counter() - start)
          seconds = elapsed % 60
          minutes = (elapsed // 60) % 60
          hours = elapsed // 3600
          status.setText(f'RUNNING rec={rec:0>3} {hours:0>2}:{minutes:0>2}:{seconds:0>2}')
          await asyncio.sleep(1)
          if isdeleted(self):
            return
      except asyncio.CancelledError:
        pass

    last_running = config.get('Running', False)
    def on_change(source, action, key, value):
      nonlocal last_running
      if key != 'Running':
        return
      if self.task:
        self.task.cancel()
        self.task = None
        status.setText('WAITING')
        status.setStyleSheet('color: red')

      if value:
        status.setStyleSheet('color: green')
        if value != last_running:
          config['start'] = time.perf_counter()
        self.task = create_task_with_exc_handling(__status_loop())
      last_running = value

    config.add_recursive_observer(on_change, lambda: isdeleted(self), True)
