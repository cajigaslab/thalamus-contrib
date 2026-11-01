import Toybox.Lang;
import Toybox.Sensor;
import Toybox.WatchUi;

// Feeds heart rate (about once a second), the accelerometer, gyroscope and
// magnetometer to a BleLink.
class SensorSource {
    // Must match MOTION_INTERVAL in rust/src/garmin_node.rs and
    // GARMIN_MOTION_INTERVAL_NS in arduino/thalamus_bridge/garmin.h.
    const MOTION_RATE_HZ = 25;
    // Gyroscope samples are sent in units of this many deg/s. Must match
    // GYRO_SCALE in rust/src/garmin_node.rs and DOUBLE_ONE_TENTH in garmin.h.
    const GYRO_SCALE = 0.1;

    var heartRate as Number? = null;
    // Why the sensors didn't start, if they didn't.
    var error as String? = null;

    private var _link as BleLink;

    function initialize(link as BleLink) {
        _link = link;
    }

    function start() as Void {
        Sensor.setEnabledSensors([Sensor.SENSOR_HEARTRATE]);
        Sensor.enableSensorEvents(method(:onSensor));
        try {
            Sensor.registerSensorDataListener(method(:onSensorData), {
                :period => 1,
                :accelerometer => { :enabled => true, :sampleRate => MOTION_RATE_HZ },
                :gyroscope => { :enabled => true, :sampleRate => MOTION_RATE_HZ },
                :magnetometer => { :enabled => true, :sampleRate => MOTION_RATE_HZ }
            });
        } catch (e) {
            error = "Sensor error";
        }
    }

    function stop() as Void {
        Sensor.unregisterSensorDataListener();
        Sensor.enableSensorEvents(null);
        Sensor.setEnabledSensors([]);
    }

    function onSensor(info as Sensor.Info) as Void {
        heartRate = info.heartRate;
        _link.sendHeartRate(heartRate != null ? heartRate : 0);
        WatchUi.requestUpdate();
    }

    function onSensorData(data as Sensor.SensorData) as Void {
        var accel = data.accelerometerData;
        if (accel != null && accel.x != null && accel.y != null && accel.z != null) {
            _link.sendVectors(BleLink.PACKET_ACCEL, accel.x, accel.y, accel.z);
        }
        var gyro = data.gyroscopeData;
        if (gyro != null && gyro.x != null && gyro.y != null && gyro.z != null) {
            _link.sendVectors(BleLink.PACKET_GYRO, scaleGyro(gyro.x), scaleGyro(gyro.y), scaleGyro(gyro.z));
        }
        var mag = data.magnetometerData;
        if (mag != null && mag.x != null && mag.y != null && mag.z != null) {
            _link.sendVectors(BleLink.PACKET_MAG, mag.x, mag.y, mag.z);
        }
    }

    private function scaleGyro(values as Array<Float>) as Array<Number> {
        var scaled = new [values.size()] as Array<Number>;
        for (var i = 0; i < values.size(); i++) {
            scaled[i] = (values[i] / GYRO_SCALE).toNumber();
        }
        return scaled;
    }
}
