use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;

use crate::api::{
    AnalogData, ImageData, MocapData, TextData, MainThreadToken, Node, NodeConsts, NodeData, NodeSelector, NodeToken, OffMainSignaler, OnDrop, State, StateAction, StateValue, THALAMUS_MODALITY_ANALOG, THALAMUS_MODALITY_IMAGE, THALAMUS_MODALITY_MOCAP, THALAMUS_MODALITY_TEXT, ThalamusAPI,
    ThalamusAPIThreadSafe,
};

/// Storage3 record format - length-prefixed protobuf
#[derive(Debug)]
struct StorageRecord {
    data: Vec<u8>,
}

impl StorageRecord {
    fn new(data: Vec<u8>) -> Self {
        StorageRecord { data }
    }
    
    fn write_to<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        // Write 8-byte big-endian length prefix
        let length = self.data.len() as u64;
        writer.write_all(&length.to_be_bytes())?;
        writer.write_all(&self.data)
    }
}

#[derive(Debug, Clone)]
struct SourceConfig {
    node: String,
    analog: bool,
    image: bool,
    motion: bool,
    text: bool,
    blob: bool,
}

#[derive(Clone)]
struct Storage3Params {
    sources: Vec<SourceConfig>,
    output: String,
    running: bool,
}

pub struct Storage3Node {
    api: ThalamusAPI,
    state: State,
    main_thread_token: MainThreadToken,
    params: Arc<Mutex<Storage3Params>>,
    state_connection: Option<OnDrop>,
    source_connections: Vec<OnDrop>,
    signaler: Arc<OffMainSignaler>,
    file_writer: Option<BufWriter<std::fs::File>>,
    notify: Arc<Notify>,
    dropping: Arc<AtomicBool>,
}

impl NodeConsts for Storage3Node {
    const MODALITIES: u32 = THALAMUS_MODALITY_ANALOG | THALAMUS_MODALITY_IMAGE | THALAMUS_MODALITY_MOCAP | THALAMUS_MODALITY_TEXT;
    const SIGNALS_OFFMAIN: bool = true;
}

impl Storage3Node {
    fn on_state(rc: Rc<RefCell<Self>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
        let mut this = rc.borrow_mut();
        match key {
            StateValue::String(ref key_str) => {
                match key_str.as_str() {
                    "Sources" => {
                        if let StateValue::List(sources) = value {
                            let mut new_sources = Vec::new();
                            for source in sources {
                                if let StateValue::Dict(source_dict) = source {
                                    let mut config = SourceConfig {
                                        node: String::new(),
                                        analog: false,
                                        image: false,
                                        motion: false,
                                        text: false,
                                        blob: false,
                                    };
                                    
                                    // Parse node name
                                    if let Some(StateValue::String(node)) = source_dict.get(&StateValue::String("Node".to_string())) {
                                        config.node = node.clone();
                                    }
                                    
                                    // Parse modality flags
                                    if let Some(StateValue::Bool(analog)) = source_dict.get(&StateValue::String("Analog".to_string())) {
                                        config.analog = *analog;
                                    }
                                    if let Some(StateValue::Bool(image)) = source_dict.get(&StateValue::String("Image".to_string())) {
                                        config.image = *image;
                                    }
                                    if let Some(StateValue::Bool(motion)) = source_dict.get(&StateValue::String("Motion".to_string())) {
                                        config.motion = *motion;
                                    }
                                    if let Some(StateValue::Bool(text)) = source_dict.get(&StateValue::String("Text".to_string())) {
                                        config.text = *text;
                                    }
                                    if let Some(StateValue::Bool(blob)) = source_dict.get(&StateValue::String("Blob".to_string())) {
                                        config.blob = *blob;
                                    }
                                    
                                    new_sources.push(config);
                                }
                            }
                            this.params.lock().unwrap().sources = new_sources;
                        }
                    },
                    "Output" => {
                        if let StateValue::String(output) = value {
                            let mut params = this.params.lock().unwrap();
                            params.output = output;
                        }
                    },
                    "Running" => {
                        if let StateValue::Bool(running) = value {
                            let mut params = this.params.lock().unwrap();
                            params.running = running;
                            
                            // Set up or tear down connections based on running state
                            if running {
                                this.setup_connections(rc.clone());
                            } else {
                                this.teardown_connections();
                            }
                        }
                    },
                    _ => {}
                }
            },
            _ => {}
        }
    }

    fn setup_connections(&mut self, rc: Rc<RefCell<Self>>) {
        self.teardown_connections();
        
        let params = self.params.lock().unwrap();
        let mut connections = Vec::new();
        
        for source_config in &params.sources {
            let node = source_config.node.clone();
            let analog = source_config.analog;
            let image = source_config.image;
            let motion = source_config.motion;
            let text = source_config.text;
            let blob = source_config.blob;
            
            let weak = Rc::downgrade(&rc);
            let api = self.api;
            let source_params = (analog, image, motion, text, blob);
            
            let temp = Some(api.get_node(NodeSelector::Name(node), move |node| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                
                // Create subscription based on source parameters
                let connection = node.subscribe_multithreaded(move |ext_node| {
                    let data = ext_node.data();
                    let time = data.time();
                    let params = source_params;
                    
                    // Record data based on selected modalities
                    if params.0 && data.analog().is_some() {
                        // Handle analog data
                        let mut this = this.borrow_mut();
                        if let Some(ref mut writer) = this.file_writer {
                            // Write analog data record
                            let analog = data.analog().unwrap();
                            // In a real implementation, you would serialize the data here
                            // For now, we're using placeholder implementation
                            let mut record_data = Vec::new();
                            record_data.extend_from_slice(b"analog_data");
                            // This would be the actual protobuf serialization
                            let record = StorageRecord::new(record_data);
                            let _ = record.write_to(writer);
                        }
                    }
                    
                    if params.1 && data.image().is_some() {
                        // Handle image data
                        let mut this = this.borrow_mut();
                        if let Some(ref mut writer) = this.file_writer {
                            // Write image data record
                            let image = data.image().unwrap();
                            // In a real implementation, you would serialize the data here
                            let mut record_data = Vec::new();
                            record_data.extend_from_slice(b"image_data");
                            // This would be the actual protobuf serialization
                            let record = StorageRecord::new(record_data);
                            let _ = record.write_to(writer);
                        }
                    }
                    
                    if params.2 && data.mocap().is_some() {
                        // Handle mocap data
                        let mut this = this.borrow_mut();
                        if let Some(ref mut writer) = this.file_writer {
                            // Write mocap data record
                            // This would be the actual protobuf serialization
                            let mut record_data = Vec::new();
                            record_data.extend_from_slice(b"mocap_data");
                            let record = StorageRecord::new(record_data);
                            let _ = record.write_to(writer);
                        }
                    }
                    
                    if params.3 && data.text().is_some() {
                        // Handle text data
                        let mut this = this.borrow_mut();
                        if let Some(ref mut writer) = this.file_writer {
                            // Write text data record
                            // This would be the actual protobuf serialization
                            let mut record_data = Vec::new();
                            record_data.extend_from_slice(b"text_data");
                            let record = StorageRecord::new(record_data);
                            let _ = record.write_to(writer);
                        }
                    }
                    
                    // Handle blob data
                    if params.4 {
                        // This would handle blob data
                        // Blob data doesn't have a direct API in this implementation
                    }
                }));
                
                connections.push(source_params);
                let _ = this.borrow_mut().source_connections.push(connection);
            }));
            
            self.source_connections.extend(temp);
        }
    }

    fn teardown_connections(&mut self) {
        self.source_connections.clear();
    }

    fn open_file(&mut self) -> std::io::Result<()> {
        let params = self.params.lock().unwrap();
        if !params.output.is_empty() {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&params.output)?;
            self.file_writer = Some(BufWriter::new(file));
        }
        Ok(())
    }

    fn close_file(&mut self) {
        self.file_writer = None;
    }
}

impl Node for Storage3Node {
    fn new(api: ThalamusAPI, node_token: NodeToken, state: State, token: MainThreadToken) -> Rc<RefCell<Self>> {
        let signaler = OffMainSignaler::new(api, node_token);
        signaler.unblock();
        
        let params = Storage3Params {
            sources: Vec::new(),
            output: String::new(),
            running: false,
        };
        
        let result = Rc::new(RefCell::new(Storage3Node {
            api,
            state: state.clone(),
            main_thread_token: token,
            params: Arc::new(Mutex::new(params)),
            state_connection: None,
            source_connections: Vec::new(),
            signaler,
            file_writer: None,
            notify: Arc::new(Notify::new()),
            dropping: Arc::new(AtomicBool::new(false)),
        }));

        let change_ref = Rc::downgrade(&result);
        let state_callback =
            move |s, a, k, v| {
                if let Some(lock) = change_ref.upgrade() {
                    Storage3Node::on_state(lock, s, a, k, v);
                };
            };

        result.borrow_mut().state_connection = Some(state.connect(state_callback));
        state.recap();
        
        {
            let borrow = result.borrow();
            let api = borrow.api.thread_safe();
            let params = borrow.params.clone();
            let notify = borrow.notify.clone();
            let dropping = borrow.dropping.clone();
            let signaler = borrow.signaler.clone();
            
            // Handle file opening in a task if needed
            api.tokio().as_ref().unwrap().spawn(async move {
                // In a real implementation, we would manage the file handling here
                // This is just the structure for the async task
            });
        }
        
        result
    }

    fn predrop(&self, token: crate::api::PredropToken) {
        self.dropping.store(true, Ordering::SeqCst);
        self.signaler.predrop(token);
        self.notify.notify_one();
    }
}

impl Drop for Storage3Node {
    fn drop(&mut self) {
        self.close_file();
    }
}