import Toybox.BluetoothLowEnergy;
import Toybox.Lang;
import Toybox.WatchUi;

// Finds the GARMIN node by its service UUID, connects, and writes packets to
// its data characteristic one at a time (Connect IQ allows one outstanding
// BLE request).
//
// Packets are at most 20 bytes (the ATT payload at the default MTU), little
// endian:
//   byte 0: type (1 = heart rate, 2 = accelerometer, 3 = gyroscope,
//           4 = magnetometer)
//   byte 1: sequence number, incremented per packet of any type, wrapping
//   heart rate:    byte 2 = bpm (0 = no reading)
//   accelerometer: 1 to 3 samples of int16 x, y, z in milli-g
//   gyroscope:     1 to 3 samples of int16 x, y, z in 0.1 deg/s
//   magnetometer:  1 to 3 samples of int16 x, y, z in milli-gauss
class BleLink extends BluetoothLowEnergy.BleDelegate {
    const PACKET_HEART_RATE = 1;
    static const PACKET_ACCEL = 2;
    static const PACKET_GYRO = 3;
    static const PACKET_MAG = 4;
    const MAX_VECTOR_SAMPLES = 3;
    // About 2 s of data. Older packets are dropped when it fills up, which
    // the node sees as a gap in sequence numbers.
    const MAX_QUEUE = 50;

    var status as String = "Starting";
    var sent as Number = 0;
    var dropped as Number = 0;
    var failed as Number = 0;

    private var _device as BluetoothLowEnergy.Device? = null;
    private var _characteristic as BluetoothLowEnergy.Characteristic? = null;
    private var _queue as Array<ByteArray> = [] as Array<ByteArray>;
    private var _writing as Boolean = false;
    private var _seq as Number = 0;

    function initialize() {
        BleDelegate.initialize();
    }

    function start() as Void {
        BluetoothLowEnergy.setDelegate(self);
        try {
            BluetoothLowEnergy.registerProfile(ThalamusProfile.definition());
            setStatus("Registering");
        } catch (e) {
            setStatus("Profile error");
        }
    }

    function stop() as Void {
        BluetoothLowEnergy.setScanState(BluetoothLowEnergy.SCAN_STATE_OFF);
        if (_device != null) {
            BluetoothLowEnergy.unpairDevice(_device);
            _device = null;
        }
        _characteristic = null;
    }

    function onProfileRegister(uuid as BluetoothLowEnergy.Uuid, status as BluetoothLowEnergy.Status) as Void {
        if (status == BluetoothLowEnergy.STATUS_SUCCESS) {
            startScan();
        } else {
            setStatus("Profile error " + status);
        }
    }

    function onScanResults(scanResults as BluetoothLowEnergy.Iterator) as Void {
        for (var result = scanResults.next(); result != null; result = scanResults.next()) {
            if (result instanceof BluetoothLowEnergy.ScanResult && advertisesService(result)) {
                BluetoothLowEnergy.setScanState(BluetoothLowEnergy.SCAN_STATE_OFF);
                try {
                    _device = BluetoothLowEnergy.pairDevice(result);
                    setStatus("Connecting");
                } catch (e) {
                    setStatus("Pair failed");
                    startScan();
                }
                return;
            }
        }
    }

    function onConnectedStateChanged(device as BluetoothLowEnergy.Device, state as BluetoothLowEnergy.ConnectionState) as Void {
        _writing = false;
        if (state != BluetoothLowEnergy.CONNECTION_STATE_CONNECTED) {
            // The system reconnects to paired devices on its own.
            _characteristic = null;
            setStatus("Reconnecting");
            return;
        }

        var service = device.getService(ThalamusProfile.serviceUuid());
        _characteristic = service != null ? service.getCharacteristic(ThalamusProfile.dataUuid()) : null;
        if (_characteristic == null) {
            setStatus("No Thalamus service");
            BluetoothLowEnergy.unpairDevice(device);
            _device = null;
            startScan();
            return;
        }
        setStatus("Connected");
        sendNext();
    }

    function onCharacteristicWrite(characteristic as BluetoothLowEnergy.Characteristic, status as BluetoothLowEnergy.Status) as Void {
        _writing = false;
        if (status == BluetoothLowEnergy.STATUS_SUCCESS) {
            sent++;
        } else {
            failed++;
        }
        sendNext();
    }

    function sendHeartRate(bpm as Number) as Void {
        var packet = new [3]b;
        packet[0] = PACKET_HEART_RATE;
        packet[1] = nextSeq();
        packet[2] = clamp(bpm, 0, 255);
        enqueue(packet);
    }

    // Sends x, y and z (equal lengths) in packets of up to MAX_VECTOR_SAMPLES
    // samples.
    function sendVectors(type as Number, x as Array<Number>, y as Array<Number>, z as Array<Number>) as Void {
        var total = x.size();
        for (var first = 0; first < total; first += MAX_VECTOR_SAMPLES) {
            var count = total - first;
            if (count > MAX_VECTOR_SAMPLES) {
                count = MAX_VECTOR_SAMPLES;
            }
            var packet = new [2 + 6 * count]b;
            packet[0] = type;
            packet[1] = nextSeq();
            for (var i = 0; i < count; i++) {
                var offset = 2 + 6 * i;
                putInt16(packet, offset, x[first + i]);
                putInt16(packet, offset + 2, y[first + i]);
                putInt16(packet, offset + 4, z[first + i]);
            }
            enqueue(packet);
        }
    }

    private function startScan() as Void {
        BluetoothLowEnergy.setScanState(BluetoothLowEnergy.SCAN_STATE_SCANNING);
        setStatus("Scanning");
    }

    private function advertisesService(result as BluetoothLowEnergy.ScanResult) as Boolean {
        var wanted = ThalamusProfile.serviceUuid().toString();
        var uuids = result.getServiceUuids();
        for (var uuid = uuids.next(); uuid != null; uuid = uuids.next()) {
            if (wanted.equals(uuid.toString())) {
                return true;
            }
        }
        return false;
    }

    private function enqueue(packet as ByteArray) as Void {
        if (_queue.size() >= MAX_QUEUE) {
            _queue = _queue.slice(1, null);
            dropped++;
        }
        _queue.add(packet);
        sendNext();
    }

    private function sendNext() as Void {
        if (_writing || _characteristic == null || _queue.size() == 0) {
            return;
        }
        try {
            _characteristic.requestWrite(_queue[0], { :writeType => BluetoothLowEnergy.WRITE_TYPE_DEFAULT });
            _queue = _queue.slice(1, null);
            _writing = true;
        } catch (e) {
            // Retried when the next packet is queued.
        }
    }

    private function nextSeq() as Number {
        var seq = _seq;
        _seq = (_seq + 1) & 0xFF;
        return seq;
    }

    private function setStatus(text as String) as Void {
        status = text;
        WatchUi.requestUpdate();
    }

    private static function putInt16(packet as ByteArray, offset as Number, value as Number) as Void {
        var v = clamp(value, -32768, 32767);
        packet[offset] = v & 0xFF;
        packet[offset + 1] = (v >> 8) & 0xFF;
    }

    private static function clamp(value as Number, low as Number, high as Number) as Number {
        if (value < low) {
            return low;
        }
        if (value > high) {
            return high;
        }
        return value;
    }
}
