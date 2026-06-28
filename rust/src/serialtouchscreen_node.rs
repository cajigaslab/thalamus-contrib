use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::{cell::RefCell, cell::Ref};
use std::time::Duration;

use crate::api::{
    State,
    StateAction,
    OnDrop,
    ThalamusAPI,
    Node,
    AnalogNode,
    StateValue,
    Json,
    TaskScope,
    run_task,
};

struct SerialTouchscreenNodeInner {
    state: State,
    state_connection: RefCell<Option<OnDrop>>,
    api: ThalamusAPI,
    samples: RefCell<Vec<f64>>,
    time: RefCell<Duration>,
    port: RefCell<String>,
    task: RefCell<Option<TaskScope>>,
}

impl SerialTouchscreenNodeInner {
    async fn serial_loop(this_weak: Weak<Self>) {
        let port = {
            let Some(this) = this_weak.upgrade() else { return; };

            {
                let mut samples = this.samples.borrow_mut();
                samples[0] = 0.0;
                samples[1] = 0.0;
            }

            let port = this.api.create_serial_port();
            port.open(&this.port.borrow()).map_err(|e| panic!("SERIAL ERROR: {}", e.message)).ok();
            port.set_baud_rate(115200).map_err(|e| panic!("SERIAL ERROR: {}", e.message)).ok();
            port
        };

        let mut byte = [0u8; 1];

        loop {
            // Scan for magic header byte 0x55
            loop {
                match port.read(&mut byte).await {
                    Err(e) if e.aborted() => return,
                    Err(e) => panic!("SERIAL ERROR: {}", e.message),
                    Ok(_) => {}
                }
                if byte[0] == 0x55 {
                    break;
                }
            }

            // Confirm second header byte 0x54
            match port.read(&mut byte).await {
                Err(e) if e.aborted() => return,
                Err(e) => panic!("SERIAL ERROR: {}", e.message),
                Ok(_) => {}
            }
            if byte[0] != 0x54 {
                continue;
            }

            // Read status (1 byte) + X little-endian u16 + Y little-endian u16
            let mut payload = [0u8; 5];
            match port.read(&mut payload).await {
                Err(e) if e.aborted() => return,
                Err(e) => panic!("SERIAL ERROR: {}", e.message),
                Ok(_) => {}
            }

            let x = u16::from_le_bytes([payload[1], payload[2]]) as f64;
            let y = u16::from_le_bytes([payload[3], payload[4]]) as f64;

            let Some(this) = this_weak.upgrade() else { return; };
            {
                let mut samples = this.samples.borrow_mut();
                samples[0] = x;
                samples[1] = y;
            }
            this.api.ready();
        }
    }

    fn on_change(self: Rc<Self>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
        let StateValue::String(key_str) = key else { return };

        match key_str.as_str() {
            "Running" => {
                if StateValue::Bool(true) == value {
                    let clone = Rc::downgrade(&self);
                    *self.task.borrow_mut() = Some(run_task(async move {
                        SerialTouchscreenNodeInner::serial_loop(clone).await;
                    }));
                } else {
                    *self.task.borrow_mut() = None;
                }
            },
            "Port" => {
                if let StateValue::String(val) = value {
                    *self.port.borrow_mut() = val;
                }
            },
            _ => {}
        }
    }
}

pub struct SerialTouchscreenNode {
    inner: Rc<SerialTouchscreenNodeInner>
}

impl AnalogNode for SerialTouchscreenNode {
    fn data(&self, channel: i32) -> impl Deref<Target = [f64]> {
        match channel {
            0 => Ref::map(self.inner.samples.borrow(), |v| &v[0..1]),
            1 => Ref::map(self.inner.samples.borrow(), |v| &v[1..2]),
            _ => Ref::map(self.inner.samples.borrow(), |v| &v[0..0]),
        }
    }

    fn num_channels(&self) -> i32 { 2 }

    fn sample_interval(&self, _channel: i32) -> Duration {
        Duration::from_millis(4)
    }

    fn name(&self, channel: i32) -> impl Deref<Target = str> {
        match channel {
            0 => "X",
            1 => "Y",
            _ => "err",
        }
    }
}

impl Node for SerialTouchscreenNode {
    fn time(&self) -> Duration {
        *self.inner.time.borrow()
    }

    fn process(&self, handle: crate::api::Request, _: Json) {
        handle.respond(&Json::from_string(self.inner.api, "{}"));
    }

    fn new(api: ThalamusAPI, state: State) -> Self {
        let inner = Rc::new(SerialTouchscreenNodeInner {
            state: state.clone(),
            state_connection: RefCell::new(None),
            api,
            samples: RefCell::new(vec![0.0, 0.0]),
            time: RefCell::new(Duration::from_millis(0)),
            port: RefCell::new("".to_string()),
            task: RefCell::new(None),
        });

        let change_ref = Rc::downgrade(&inner);
        let callback = move |source: State, action: StateAction, key: StateValue, value: StateValue| {
            change_ref.upgrade().map(|val| {
                val.on_change(source, action, key, value);
            });
        };
        {
            let mut temp = inner.state_connection.borrow_mut();
            *temp = Some(inner.state.connect(callback));
        }
        state.recap();

        SerialTouchscreenNode { inner }
    }
}

impl Drop for SerialTouchscreenNode {
    fn drop(&mut self) {
        println!("SerialTouchscreenNode.drop");
    }
}
