use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::{cell::RefCell, cell::Ref};
use std::time::Duration;

use futures::select;

use crate::api::{
    State,
    OnDrop,
    ThalamusAPI,
    Node,
    AnalogNode,
    StateValue,
    Json,
    TaskScope,
    run_task,
};

struct JoystickNodeInner {
  state: State,
  state_connection: RefCell<Option<OnDrop>>,
  api: ThalamusAPI,
  samples: RefCell<Vec<f64>>,
  processed: RefCell<Vec<f64>>,
  time: RefCell<Duration>,
  port: RefCell<String>,
  task: RefCell<Option<TaskScope>>,
  invert_x: RefCell<bool>,
  invert_y: RefCell<bool>,
  x_center: RefCell<f64>,
  y_center: RefCell<f64>,
  dead_zone: RefCell<f64>,
}

impl JoystickNodeInner {
  fn update_processed(&self) {
    let samples = self.samples.borrow();
    let mut processed = self.processed.borrow_mut();
    let x_center = *self.x_center.borrow();
    let y_center = *self.y_center.borrow();
    let dead_zone = *self.dead_zone.borrow();
    let invert_x = *self.invert_x.borrow();
    let invert_y = *self.invert_y.borrow();

    let mut x = samples[0];
    let mut y = samples[1];

    if (x - x_center).abs() < dead_zone { x = x_center; }
    if (y - y_center).abs() < dead_zone { y = y_center; }

    x = x.clamp(0.0, 1923.0);
    y = y.clamp(0.0, 1923.0);

    x = (x - x_center) / 512.0;
    y = (y - y_center) / 512.0;

    if invert_x { x = -x; }
    if invert_y { y = -y; }

    processed[0] = x;
    processed[1] = y;
  }

  async fn serial_loop(this_weak: Weak<Self>) {
    let (port, buffer, timer) = {
      let Some(this) = this_weak.upgrade() else {
        return;
      };

      {
        let mut samples = this.samples.borrow_mut();
        samples.clear();
        samples.push(0.0);
        samples.push(0.0);
      }
      this.update_processed();

      let port = this.api.create_serial_port();
      port.open(&this.port.borrow()).map_err(|e| { panic!("SERIAL ERROR: {}", e.message) } );
      port.set_baud_rate(115200).map_err(|e| { panic!("SERIAL ERROR: {}", e.message) } );

      let buffer = this.api.create_streambuf();

      let timer = this.api.create_timer();
      (port, buffer, timer)
    };

    //let re = Regex::new(r"(x\s*=\s*(\d+)\s*,\s*y\s*=\s*(\d+))").unwrap();

    let mut sleep = timer.sleep(Duration::from_millis(16));
    let mut read = port.read_until(&buffer, "\n");

    loop {
      println!("Read");
      select! {
        _ = sleep => {
          let Some(this) = this_weak.upgrade() else {
            return;
          };
          this.api.ready();
          sleep = timer.sleep(Duration::from_millis(16));
        },
        result = read => {
          match result {
            Err(err) => {
              println!("Err {}", err.message);
              if err.aborted() {
                return;
              }
              panic!("SERIAL ERROR: {}", err.message);
            },
            Ok(count) => {
              println!("Ok {}", count);
              let line = buffer.to_string();
              buffer.consume(buffer.size());

              println!("{}", line);
              let parts = line.trim().split(",");
              let numbers: Vec<f64> = parts
                  .map(|t| t.parse::<f64>())
                  .filter(|p| p.is_ok())
                  .map(|p| p.unwrap())
                  .collect();
              println!("numbers {:?}", numbers);

              let Some(this) = this_weak.upgrade() else {
                return;
              };

              if numbers.len() == 2 {
                {
                  let mut samples = this.samples.borrow_mut();
                  samples.clear();
                  samples.push(numbers[0]);
                  samples.push(numbers[1]);
                }
                this.update_processed();
                println!("processed {:?}", this.processed.borrow());
              }
            }
          }
          read = port.read_until(&buffer, "\n");
        },
      };
    }
  }

  fn on_change(self: Rc<Self>, _source: &State, _action: i32, key: StateValue, value: StateValue) {
    println!("DemoNode::on_change {:?} {:?}", key, value);
    let StateValue::String(key_str) = key else {
      return
    };

    match key_str.as_str() {
      "Running" => {
        if StateValue::Bool(true) == value {
          let clone = Rc::downgrade(&self);
          *self.task.borrow_mut() = Some(run_task(async move {
            JoystickNodeInner::serial_loop(clone).await;
          }));
        } else {
          *self.task.borrow_mut() = None
        }
      },
      "Port" => {
        if let StateValue::String(val) = value {
          *self.port.borrow_mut() = val;
        };
      },
      "Invert X" => {
        if let StateValue::Bool(val) = value {
          *self.invert_x.borrow_mut() = val;
        }
        self.update_processed();
      },
      "Invert Y" => {
        if let StateValue::Bool(val) = value {
          *self.invert_y.borrow_mut() = val;
        }
        self.update_processed();
      },
      "X Center" => {
        *self.x_center.borrow_mut() = f64::try_from(value).unwrap();
        self.update_processed();
      },
      "Y Center" => {
        *self.y_center.borrow_mut() = f64::try_from(value).unwrap();
        self.update_processed();
      },
      "Dead Zone" => {
        *self.dead_zone.borrow_mut() = f64::try_from(value).unwrap();
        self.update_processed();
      },
      _ => {}
    }
  }
}

pub struct JoystickNode {
  inner: Rc<JoystickNodeInner>
}

impl AnalogNode for JoystickNode {
  fn data(&self, channel: i32) -> impl Deref<Target = [f64]> {
      match channel {
          0 => Ref::map(self.inner.processed.borrow(), |v| &v[0..1]),
          1 => Ref::map(self.inner.processed.borrow(), |v| &v[1..2]),
          _ => Ref::map(self.inner.processed.borrow(), |v| &v[0..0]),
      }
  }

  fn num_channels(&self) -> i32 { 2 }

  fn sample_interval(&self, _channel: i32) -> Duration {
    Duration::from_millis(16)
  }

  fn name(&self, channel: i32) -> impl Deref<Target = str> {
      match channel {
          0 => "X",
          1 => "Y",
          _ => "err"
      }
  }
}

impl Node for JoystickNode {
  fn time(&self) -> Duration {
    *self.inner.time.borrow()
  }

  fn process(&self, handle: crate::api::Request, _: Json) {
    handle.respond(&Json::from_string(self.inner.api, "{}"));
  }

  fn new(api: ThalamusAPI, state: State) -> Self {
    let inner = Rc::new(JoystickNodeInner {
      state: state.clone(),
      state_connection: RefCell::new(None),
      api,
      samples: RefCell::new(vec![0.0, 0.0]),
      processed: RefCell::new(vec![0.0, 0.0]),
      time: RefCell::new(Duration::from_millis(0)),
      port: RefCell::new("".to_string()),
      task: RefCell::new(None),
      invert_x: RefCell::new(false),
      invert_y: RefCell::new(false),
      x_center: RefCell::new(512.0),
      y_center: RefCell::new(512.0),
      dead_zone: RefCell::new(3.0),
    });

    let change_ref = Rc::downgrade(&inner);
    let callback = move |source: &State, action: i32, key: StateValue, value: StateValue| {
      change_ref.upgrade().map(|val| {
        val.on_change(source, action, key, value);
      });
    };
    {
      let mut temp = inner.state_connection.borrow_mut();
      *temp = Some(inner.state.connect(callback));
    }
    state.recap();

    JoystickNode { inner }
  }
}

impl Drop for JoystickNode {
  fn drop(&mut self) {
    println!("SerialNode.drop");
    //self.inner.task.borrow_mut().take().map(|t| { drop(t) });
  }
}
